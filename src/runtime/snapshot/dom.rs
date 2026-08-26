use serde::Deserialize;

use super::{DocumentLoadState, ElementBounds, SnapshotTruncation, ViewportSnapshot};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentFacts {
    pub url: Option<String>,
    pub title: Option<String>,
    pub required_fields_fit: bool,
    pub load_state: DocumentLoadState,
    pub visible_text: String,
    pub viewport: ViewportSnapshot,
    pub truncation: SnapshotTruncation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ElementFacts {
    pub tag_name: String,
    pub id: Option<String>,
    pub test_id: Option<String>,
    pub text: String,
    pub bounds: ElementBounds,
    pub focused: bool,
    pub omitted_bytes: usize,
}

pub(super) fn document_expression(max_bytes: usize) -> String {
    format!("({DOCUMENT_FACTS_FUNCTION})({max_bytes})")
}

pub(super) fn candidates_expression(max_elements: usize, include_root: bool) -> String {
    format!(
        "({CANDIDATES_FUNCTION})({max_elements}, {})",
        if include_root { "true" } else { "false" }
    )
}

pub(super) const DOCUMENT_FACTS_FUNCTION: &str = r#"function(maxBytes) {
  const encoder = new TextEncoder();
  let remaining = maxBytes;
  let visibleText = '';
  let emittedBytes = 0;
  let omittedBytes = 0;
  let sawText = false;
  const takeRequired = value => {
    let output = '';
    for (const character of String(value || '')) {
      const bytes = encoder.encode(character).length;
      if (bytes > remaining) { omittedBytes += bytes; return null; }
      output += character; remaining -= bytes; emittedBytes += bytes;
    }
    return output;
  };
  const url = takeRequired(location.href);
  const title = url === null ? null : takeRequired(document.title);
  const requiredFieldsFit = url !== null && title !== null;
  const emit = character => {
    const bytes = encoder.encode(character).length;
    if (bytes > remaining) omittedBytes += bytes;
    else { visibleText += character; emittedBytes += bytes; remaining -= bytes; }
  };
  const append = value => {
    let pendingSpace = sawText;
    let appended = false;
    for (const character of String(value || '')) {
      if (/\s/u.test(character)) {
        if (sawText || appended) pendingSpace = true;
      } else {
        if (pendingSpace) emit(' ');
        emit(character);
        sawText = true;
        appended = true;
        pendingSpace = false;
      }
    }
  };
  const walk = node => {
    if (node.nodeType === Node.TEXT_NODE) { append(node.nodeValue); return; }
    if (node.nodeType === Node.ELEMENT_NODE) {
      const style = getComputedStyle(node);
      if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) return;
    }
    for (const child of node.childNodes || []) walk(child);
    if (node.shadowRoot) walk(node.shadowRoot);
  };
  if (requiredFieldsFit) walk(document.documentElement);
  const scrolling = document.scrollingElement || document.documentElement;
  return {
    url, title, requiredFieldsFit, loadState: document.readyState, visibleText,
    viewport: { width: innerWidth, height: innerHeight, scrollX, scrollY, documentWidth: scrolling.scrollWidth, documentHeight: scrolling.scrollHeight },
    truncation: { maxBytes, maxElements: 0, emittedBytes, emittedElements: 0, omittedBytes, omittedElements: 0, omittedFrames: 0, unavailableAccessibility: 0 }
  };
}"#;

pub(super) const CANDIDATES_FUNCTION: &str = r#"function(maxElements, includeRoot) {
  const root = this && this.nodeType === Node.ELEMENT_NODE ? this : document.documentElement;
  const composedParent = node => node.parentElement || node.getRootNode()?.host || null;
  const visible = element => {
    if (!element || !element.isConnected) return false;
    for (let current = element; current; current = composedParent(current)) {
      const style = getComputedStyle(current);
      if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) return false;
    }
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };
  const interactive = element => element.matches('a[href],button,input,select,textarea,summary,[role],[tabindex],[contenteditable=true]');
  const nodes = [];
  let total = 0;
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  let sawActive = false;
  const add = element => {
    if (visible(element)) {
      if (element === active) sawActive = true;
      total += 1;
      if (nodes.length < maxElements) nodes.push(element);
    }
  };
  if (includeRoot) add(root);
  const walk = container => {
    if (container.shadowRoot) walk(container.shadowRoot);
    for (const element of container.children || []) {
      if (interactive(element)) add(element);
      walk(element);
    }
  };
  walk(root);
  const withinRoot = element => {
    for (let current = element; current; current = composedParent(current)) if (current === root) return true;
    return false;
  };
  if (active && !sawActive && withinRoot(active)) add(active);
  Object.defineProperty(nodes, '__browserkitTotal', { value: total, enumerable: true });
  return nodes;
}"#;

pub(super) const ELEMENT_FACTS_FUNCTION: &str = r#"function(maxBytes) {
  const encoder = new TextEncoder();
  let remaining = maxBytes;
  let omittedBytes = 0;
  const takeAttribute = value => {
    let output = '';
    for (const character of String(value || '')) {
      const bytes = encoder.encode(character).length;
      if (bytes <= remaining) { output += character; remaining -= bytes; }
      else omittedBytes += bytes;
    }
    return output || null;
  };
  let text = '';
  let sawText = false;
  const emitText = character => {
    const bytes = encoder.encode(character).length;
    if (bytes <= remaining) { text += character; remaining -= bytes; }
    else omittedBytes += bytes;
  };
  const append = value => {
    let pendingSpace = sawText;
    let appended = false;
    for (const character of String(value || '')) {
      if (/\s/u.test(character)) {
        if (sawText || appended) pendingSpace = true;
      } else {
        if (pendingSpace) emitText(' ');
        emitText(character);
        sawText = true;
        appended = true;
        pendingSpace = false;
      }
    }
  };
  const walkText = node => {
    if (node.nodeType === Node.TEXT_NODE) { append(node.nodeValue); return; }
    if (node.nodeType === Node.ELEMENT_NODE) {
      const style = getComputedStyle(node);
      if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) return;
    }
    if (node.shadowRoot) walkText(node.shadowRoot);
    for (const child of node.childNodes || []) walkText(child);
  };
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  const rect = this.getBoundingClientRect();
  const id = takeAttribute(this.id);
  const testId = takeAttribute(this.getAttribute('data-testid'));
  walkText(this);
  return {
    tagName: this.localName, id, testId, text, focused: active === this, omittedBytes,
    bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
  };
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_collectors_do_not_materialize_unbounded_sets_or_full_text_values() {
        assert!(!CANDIDATES_FUNCTION.contains("new Set"));
        for collector in [DOCUMENT_FACTS_FUNCTION, ELEMENT_FACTS_FUNCTION] {
            assert!(!collector.contains("textContent"));
            assert!(!collector.contains("innerText"));
            assert!(!collector.contains("replace(/\\s+"));
        }
        assert!(!DOCUMENT_FACTS_FUNCTION.contains("url: location.href"));
        assert!(!DOCUMENT_FACTS_FUNCTION.contains("title: document.title"));
    }
}
