use crate::runtime::{
    BrowserError, Frame, InvalidationReason, LifecycleSnapshot, Page, PageLifecycle,
};

// These Task 2 internals become non-test call paths when Task 4 adds actions.
#[allow(dead_code)]
pub(super) mod actionability;
#[allow(dead_code)]
pub(super) mod resolver;

/// A query used to resolve elements within a page, frame, or another locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorQuery {
    Css(String),
    XPath(String),
    Text(TextMatcher),
    Role(RoleQuery),
    Label(TextMatcher),
    Placeholder(TextMatcher),
    TestId(TestIdQuery),
}

impl LocatorQuery {
    pub fn css(selector: impl Into<String>) -> Self {
        Self::Css(selector.into())
    }

    pub fn xpath(expression: impl Into<String>) -> Self {
        Self::XPath(expression.into())
    }

    pub fn text(matcher: TextMatcher) -> Self {
        Self::Text(matcher)
    }

    pub fn role(query: RoleQuery) -> Self {
        Self::Role(query)
    }

    pub fn label(matcher: TextMatcher) -> Self {
        Self::Label(matcher)
    }

    pub fn placeholder(matcher: TextMatcher) -> Self {
        Self::Placeholder(matcher)
    }

    pub fn test_id(query: TestIdQuery) -> Self {
        Self::TestId(query)
    }

    pub fn as_role(&self) -> Option<&RoleQuery> {
        match self {
            Self::Role(query) => Some(query),
            _ => None,
        }
    }

    pub fn as_test_id(&self) -> Option<&TestIdQuery> {
        match self {
            Self::TestId(query) => Some(query),
            _ => None,
        }
    }
}

impl From<String> for LocatorQuery {
    fn from(selector: String) -> Self {
        Self::Css(selector)
    }
}

impl From<&str> for LocatorQuery {
    fn from(selector: &str) -> Self {
        Self::Css(selector.to_owned())
    }
}

/// A text predicate. Regular expressions are stored for resolution time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextMatcher {
    Exact {
        value: String,
        case_sensitive: bool,
    },
    Contains {
        value: String,
        case_sensitive: bool,
    },
    Regex {
        pattern: String,
        case_sensitive: bool,
    },
}

impl TextMatcher {
    pub fn exact(value: impl Into<String>, case_sensitive: bool) -> Self {
        Self::Exact {
            value: value.into(),
            case_sensitive,
        }
    }

    pub fn contains(value: impl Into<String>, case_sensitive: bool) -> Self {
        Self::Contains {
            value: value.into(),
            case_sensitive,
        }
    }

    pub fn regex(pattern: impl Into<String>, case_sensitive: bool) -> Self {
        Self::Regex {
            pattern: pattern.into(),
            case_sensitive,
        }
    }
}

/// An accessibility-role query with an optional accessible-name predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleQuery {
    role: String,
    name: Option<TextMatcher>,
}

impl RoleQuery {
    pub fn new(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            name: None,
        }
    }

    pub fn with_name(mut self, name: TextMatcher) -> Self {
        self.name = Some(name);
        self
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn name(&self) -> Option<&TextMatcher> {
        self.name.as_ref()
    }
}

/// A query against the conventional `data-testid` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestIdQuery {
    value: String,
}

impl TestIdQuery {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

/// How a locator selects from the matches produced by its query chain.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LocatorMatch {
    #[default]
    Strict,
    First,
    Last,
    Nth(usize),
}

#[derive(Debug, Clone)]
struct LocatorPlan {
    queries: Vec<LocatorQuery>,
    match_policies: Vec<LocatorMatch>,
}

impl LocatorPlan {
    fn new(query: LocatorQuery) -> Self {
        Self {
            queries: vec![query],
            match_policies: vec![LocatorMatch::Strict],
        }
    }

    fn descendant(&self, query: LocatorQuery) -> Self {
        let mut plan = self.clone();
        plan.queries.push(query);
        plan.match_policies.push(LocatorMatch::Strict);
        plan
    }

    fn first(&self) -> Self {
        self.with_match(LocatorMatch::First)
    }

    fn last(&self) -> Self {
        self.with_match(LocatorMatch::Last)
    }

    fn nth(&self, index: usize) -> Self {
        self.with_match(LocatorMatch::Nth(index))
    }

    fn with_match(&self, match_policy: LocatorMatch) -> Self {
        let mut plan = self.clone();
        *plan
            .match_policies
            .last_mut()
            .expect("a locator plan always has one query") = match_policy;
        plan
    }

    fn queries(&self) -> &[LocatorQuery] {
        &self.queries
    }

    fn match_policy(&self) -> LocatorMatch {
        *self
            .match_policies
            .last()
            .expect("a locator plan always has one query")
    }

    #[allow(dead_code)]
    fn match_policies(&self) -> &[LocatorMatch] {
        &self.match_policies
    }
}

#[derive(Debug, Clone, Copy)]
struct LocatorDocumentScope {
    snapshot: LifecycleSnapshot,
}

impl LocatorDocumentScope {
    fn capture(lifecycle: &PageLifecycle) -> Self {
        Self {
            snapshot: lifecycle.snapshot(),
        }
    }

    fn validate(self, lifecycle: &PageLifecycle) -> Result<(), InvalidationReason> {
        lifecycle.validate_document(self.snapshot)
    }
}

#[derive(Clone)]
enum LocatorRoot {
    Page {
        page: Page,
        scope: LocatorDocumentScope,
    },
    Frame(Frame),
}

impl std::fmt::Debug for LocatorRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Page { page, scope } => formatter
                .debug_struct("PageLocatorRoot")
                .field("page", page)
                .field("scope", scope)
                .finish(),
            Self::Frame(frame) => formatter
                .debug_tuple("FrameLocatorRoot")
                .field(frame)
                .finish(),
        }
    }
}

impl LocatorRoot {
    #[allow(dead_code)] // Used by Task 2 resolution before Task 4 wires actions to it.
    fn validate_locator_document(&self) -> Result<(), BrowserError> {
        match self {
            Self::Page { page, scope } => scope.validate(page.lifecycle()).map_err(|reason| {
                BrowserError::operation("use locator", super::OperationPhase::Preparation)
                    .with_message(format!("locator is stale: {reason:?}"))
            }),
            Self::Frame(_) => Ok(()),
        }
    }

    #[allow(dead_code)] // Used by Task 2 resolution before Task 4 wires actions to it.
    fn page(&self) -> &Page {
        match self {
            Self::Page { page, .. } => page,
            Self::Frame(frame) => frame.page(),
        }
    }

    #[allow(dead_code)] // Used by Task 2 resolution before Task 4 wires actions to it.
    fn frame_route(
        &self,
        store: &super::FrameStore,
    ) -> Result<super::frame::LocatorFrameRoute, BrowserError> {
        match self {
            Self::Page { .. } => {
                let id = store.main_frame_id().ok_or_else(|| {
                    BrowserError::operation(
                        "resolve locator frame route",
                        super::OperationPhase::Preparation,
                    )
                    .with_message("page has no main frame")
                })?;
                let frame = store.handle(&id).ok_or_else(|| {
                    BrowserError::operation(
                        "resolve locator frame route",
                        super::OperationPhase::Preparation,
                    )
                    .with_message("page main frame disappeared")
                })?;
                store.locator_route(&frame)
            }
            Self::Frame(frame) => store.locator_route(frame),
        }
    }

    async fn validate(&self) -> Result<(), BrowserError> {
        match self {
            Self::Page { page, scope } => scope.validate(page.lifecycle()).map_err(|reason| {
                BrowserError::operation("use locator", super::OperationPhase::Preparation)
                    .with_message(format!("locator is stale: {reason:?}"))
            }),
            Self::Frame(frame) => frame.validate_locator_scope().await,
        }
    }
}

/// A lazy, document-scoped element query. It never retains a DOM node.
#[derive(Debug, Clone)]
pub struct Locator {
    root: LocatorRoot,
    plan: LocatorPlan,
}

impl Locator {
    pub async fn screenshot(
        &self,
        options: super::ScreenshotOptions,
    ) -> Result<super::ArtifactBytes, BrowserError> {
        super::artifact::screenshot_locator(self, options).await
    }

    pub(super) fn page(&self) -> &Page {
        self.root.page()
    }

    pub async fn wait(
        &self,
        condition: super::LocatorCondition,
        options: super::WaitOptions,
    ) -> Result<(), BrowserError> {
        super::wait::wait_locator(self, condition, options).await
    }

    pub async fn click(&self) -> Result<(), BrowserError> {
        super::action::locator_click(self, 1).await
    }
    pub async fn double_click(&self) -> Result<(), BrowserError> {
        super::action::locator_click(self, 2).await
    }
    pub async fn fill(&self, value: &str) -> Result<(), BrowserError> {
        super::action::locator_fill(self, value).await
    }
    pub async fn type_text(&self, value: &str) -> Result<(), BrowserError> {
        super::action::locator_type_text(self, value).await
    }
    pub async fn press(&self, key: &str) -> Result<(), BrowserError> {
        super::action::locator_press(self, key).await
    }
    pub async fn select<I, S>(&self, values: I) -> Result<(), BrowserError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        super::action::locator_select(self, values).await
    }
    pub async fn check(&self) -> Result<(), BrowserError> {
        super::action::locator_set_checked(self, true).await
    }
    pub async fn uncheck(&self) -> Result<(), BrowserError> {
        super::action::locator_set_checked(self, false).await
    }
    pub async fn hover(&self) -> Result<(), BrowserError> {
        super::action::locator_hover(self).await
    }
    pub async fn focus(&self) -> Result<(), BrowserError> {
        super::action::locator_focus(self).await
    }
    pub async fn blur(&self) -> Result<(), BrowserError> {
        super::action::locator_blur(self).await
    }
    pub async fn scroll(&self, delta_x: f64, delta_y: f64) -> Result<(), BrowserError> {
        super::action::locator_scroll(self, delta_x, delta_y).await
    }
    pub async fn scroll_into_view(&self) -> Result<(), BrowserError> {
        super::action::locator_scroll_into_view(self).await
    }
    pub async fn drag_to(&self, target: &Self) -> Result<(), BrowserError> {
        super::action::locator_drag_to(self, target).await
    }
    pub async fn set_input_files<I, S>(&self, files: I) -> Result<(), BrowserError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        super::action::locator_set_input_files(self, files).await
    }

    /// Captures a bounded structured snapshot rooted at the resolved element.
    pub async fn snapshot(
        &self,
        options: super::SnapshotOptions,
    ) -> Result<super::ElementSnapshot, BrowserError> {
        super::snapshot::capture_locator(self, options).await
    }

    pub(super) fn page_for_snapshot(&self) -> &Page {
        self.root.page()
    }

    pub(super) fn page_for_action(&self) -> &Page {
        self.root.page()
    }

    pub(super) fn validate_document_for_action(&self) -> Result<(), BrowserError> {
        self.root.validate_locator_document()
    }

    pub(crate) fn for_page(page: Page, query: LocatorQuery) -> Self {
        let scope = LocatorDocumentScope::capture(page.lifecycle());
        Self {
            root: LocatorRoot::Page { page, scope },
            plan: LocatorPlan::new(query),
        }
    }

    pub(crate) fn for_frame(frame: Frame, query: LocatorQuery) -> Self {
        Self {
            root: LocatorRoot::Frame(frame),
            plan: LocatorPlan::new(query),
        }
    }

    pub fn locator(&self, query: impl Into<LocatorQuery>) -> Self {
        Self {
            root: self.root.clone(),
            plan: self.plan.descendant(query.into()),
        }
    }

    pub fn first(&self) -> Self {
        Self {
            root: self.root.clone(),
            plan: self.plan.first(),
        }
    }

    pub fn last(&self) -> Self {
        Self {
            root: self.root.clone(),
            plan: self.plan.last(),
        }
    }

    pub fn nth(&self, index: usize) -> Self {
        Self {
            root: self.root.clone(),
            plan: self.plan.nth(index),
        }
    }

    pub fn queries(&self) -> &[LocatorQuery] {
        self.plan.queries()
    }

    pub fn match_policy(&self) -> LocatorMatch {
        self.plan.match_policy()
    }

    #[allow(dead_code)]
    pub(crate) async fn validate_scope(&self) -> Result<(), BrowserError> {
        self.root.validate().await
    }

    #[allow(dead_code)] // Task 4 actions will become the production callers.
    pub(super) async fn resolve_admitted<'operation>(
        &self,
        operation: &'operation super::page::PageOperation,
    ) -> Result<resolver::ResolvedElement<'operation>, BrowserError> {
        self.root.validate_locator_document()?;
        let page = self.root.page();
        let store = page.locator_frame_store(operation).await?;
        let route = self.root.frame_route(store)?;
        let resolved = resolver::resolve(page, store, &route, &self.plan, operation).await;
        let validation = self
            .root
            .validate_locator_document()
            .and_then(|()| store.validate_locator_route(&route));
        match (resolved, validation) {
            (Ok(resolved), Ok(())) => Ok(resolved),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(stale)) => Err(stale),
            (Err(error), Err(stale)) => Err(stale.with_cleanup_failures_from(&error)),
        }
    }

    pub(super) async fn count_admitted(
        &self,
        operation: &super::page::PageOperation,
    ) -> Result<usize, BrowserError> {
        self.root.validate_locator_document()?;
        let page = self.root.page();
        let store = page.locator_frame_store(operation).await?;
        let route = self.root.frame_route(store)?;
        let count = resolver::count(store, &route, &self.plan).await;
        let validation = self
            .root
            .validate_locator_document()
            .and_then(|()| store.validate_locator_route(&route));
        match (count, validation) {
            (Ok(count), Ok(())) => Ok(count),
            (Err(error), Ok(())) => Err(error),
            (_, Err(stale)) => Err(stale),
        }
    }

    #[cfg(test)]
    async fn resolve_for_test(&self) -> Result<resolver::ResolutionSummary, BrowserError> {
        let operation = self.root.page().admit_operation("resolve locator")?;
        let resolved = self.resolve_admitted(&operation).await?;
        resolved.facts.ensure_actionable()?;
        Ok(resolver::ResolutionSummary {
            backend_node_id: resolved.backend_node_id,
            session_id: resolved.session.id().to_owned(),
            facts: resolved.facts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{InvalidationReason, PageGeneration, PageLifecycle};
    use static_assertions::assert_impl_all;

    assert_impl_all!(Locator: Clone, Send, Sync);

    #[test]
    fn query_constructors_cover_the_public_locator_surface() {
        assert_eq!(
            LocatorQuery::css("main > button"),
            LocatorQuery::Css("main > button".to_owned())
        );
        assert_eq!(
            LocatorQuery::xpath("//button"),
            LocatorQuery::XPath("//button".to_owned())
        );
        assert_eq!(
            LocatorQuery::text(TextMatcher::exact("Save", true)),
            LocatorQuery::Text(TextMatcher::Exact {
                value: "Save".to_owned(),
                case_sensitive: true,
            })
        );
        assert_eq!(
            LocatorQuery::label(TextMatcher::contains("email", false)),
            LocatorQuery::Label(TextMatcher::Contains {
                value: "email".to_owned(),
                case_sensitive: false,
            })
        );
        assert_eq!(
            LocatorQuery::placeholder(TextMatcher::regex("^user@", false)),
            LocatorQuery::Placeholder(TextMatcher::Regex {
                pattern: "^user@".to_owned(),
                case_sensitive: false,
            })
        );
        assert_eq!(
            LocatorQuery::role(
                RoleQuery::new("button").with_name(TextMatcher::exact("Submit", false,))
            ),
            LocatorQuery::Role(
                RoleQuery::new("button").with_name(TextMatcher::exact("Submit", false,))
            )
        );
        assert_eq!(
            LocatorQuery::test_id(TestIdQuery::new("checkout")),
            LocatorQuery::TestId(TestIdQuery::new("checkout"))
        );
        assert_eq!(
            LocatorQuery::from("article"),
            LocatorQuery::Css("article".to_owned())
        );
    }

    #[test]
    fn locator_plan_is_strict_until_an_ordinal_is_selected() {
        let strict = LocatorPlan::new(LocatorQuery::css("button"));
        assert_eq!(strict.match_policy(), LocatorMatch::Strict);

        assert_eq!(strict.first().match_policy(), LocatorMatch::First);
        assert_eq!(strict.last().match_policy(), LocatorMatch::Last);
        assert_eq!(strict.nth(4).match_policy(), LocatorMatch::Nth(4));
        assert_eq!(strict.match_policy(), LocatorMatch::Strict);
    }

    #[test]
    fn descendant_filtering_appends_queries_without_mutating_the_parent() {
        let parent = LocatorPlan::new(LocatorQuery::role(RoleQuery::new("dialog")));
        let child = parent.descendant(LocatorQuery::text(TextMatcher::contains("Confirm", true)));

        assert_eq!(parent.queries().len(), 1);
        assert_eq!(child.queries().len(), 2);
        assert_eq!(child.match_policy(), LocatorMatch::Strict);
    }

    #[test]
    fn descendant_filtering_preserves_the_parent_ordinal() {
        let rows = LocatorPlan::new(LocatorQuery::css(".row")).first();
        let button = rows.descendant(LocatorQuery::css("button"));

        assert_eq!(
            button.match_policies(),
            &[LocatorMatch::First, LocatorMatch::Strict]
        );
    }

    #[test]
    fn document_scope_fails_closed_after_navigation_or_target_replacement() {
        let lifecycle = PageLifecycle::new(PageGeneration::initial());
        let original = LocatorDocumentScope::capture(&lifecycle);

        assert_eq!(original.validate(&lifecycle), Ok(()));
        lifecycle.commit_new_document();
        assert_eq!(
            original.validate(&lifecycle),
            Err(InvalidationReason::DocumentChanged)
        );

        let navigated = LocatorDocumentScope::capture(&lifecycle);
        lifecycle.replace_target();
        assert_eq!(
            navigated.validate(&lifecycle),
            Err(InvalidationReason::PageReplaced)
        );
    }
}
