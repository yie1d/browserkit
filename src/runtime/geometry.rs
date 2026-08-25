use std::marker::PhantomData;
use std::sync::Arc;

use cdpkit::dom::methods::{GetBoxModel, GetFrameOwner};
use cdpkit::page::methods::GetLayoutMetrics;
use cdpkit::runtime::methods::Evaluate as CdpEvaluate;
use serde::Deserialize;

use super::frame::{LocatorFrameRoute, MainWorldContext};
use super::{BrowserError, OperationPhase, Page};

const EPSILON: f64 = 1e-7;
const SCALE_EPSILON: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameViewport {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionViewport {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopViewport {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopPage {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Point<Space> {
    x: f64,
    y: f64,
    space: PhantomData<Space>,
}

impl<Space> Point<Space> {
    pub(crate) fn new(x: f64, y: f64, operation: &'static str) -> Result<Self, BrowserError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(geometry_error(operation, "geometry point is non-finite"));
        }
        Ok(Self {
            x,
            y,
            space: PhantomData,
        })
    }

    pub(crate) fn x(self) -> f64 {
        self.x
    }

    pub(crate) fn y(self) -> f64 {
        self.y
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Size<Space> {
    width: f64,
    height: f64,
    space: PhantomData<Space>,
}

impl<Space> Size<Space> {
    fn new(width: f64, height: f64, operation: &'static str) -> Result<Self, BrowserError> {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(geometry_error(
                operation,
                "geometry aperture has non-finite or non-positive dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            space: PhantomData,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Quad<Space> {
    coordinates: [f64; 8],
    space: PhantomData<Space>,
}

impl<Space> Quad<Space> {
    pub(crate) fn try_from_slice(
        coordinates: &[f64],
        operation: &'static str,
    ) -> Result<Self, BrowserError> {
        let coordinates: [f64; 8] = coordinates
            .try_into()
            .map_err(|_| geometry_error(operation, "geometry quad must contain four points"))?;
        if !coordinates.iter().all(|coordinate| coordinate.is_finite()) {
            return Err(geometry_error(operation, "geometry quad is non-finite"));
        }
        Ok(Self {
            coordinates,
            space: PhantomData,
        })
    }

    #[cfg(test)]
    pub(crate) fn coordinates(self) -> [f64; 8] {
        self.coordinates
    }

    pub(crate) fn center(self, operation: &'static str) -> Result<Point<Space>, BrowserError> {
        Point::new(
            (self.coordinates[0] + self.coordinates[2] + self.coordinates[4] + self.coordinates[6])
                / 4.0,
            (self.coordinates[1] + self.coordinates[3] + self.coordinates[5] + self.coordinates[7])
                / 4.0,
            operation,
        )
    }

    pub(crate) fn bounds(self, operation: &'static str) -> Result<Bounds<Space>, BrowserError> {
        let xs = [
            self.coordinates[0],
            self.coordinates[2],
            self.coordinates[4],
            self.coordinates[6],
        ];
        let ys = [
            self.coordinates[1],
            self.coordinates[3],
            self.coordinates[5],
            self.coordinates[7],
        ];
        let min_x = xs.into_iter().fold(f64::INFINITY, f64::min);
        let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max);
        let min_y = ys.into_iter().fold(f64::INFINITY, f64::min);
        let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max);
        Bounds::new(min_x, min_y, max_x - min_x, max_y - min_y, operation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bounds<Space> {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
    space: PhantomData<Space>,
}

impl<Space> Bounds<Space> {
    fn new(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        operation: &'static str,
    ) -> Result<Self, BrowserError> {
        if ![x, y, width, height].into_iter().all(f64::is_finite) || width <= 0.0 || height <= 0.0 {
            return Err(geometry_error(
                operation,
                "geometry bounds are empty or non-finite",
            ));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
            space: PhantomData,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct FrameViewportPayload {
    inner_width: f64,
    inner_height: f64,
    scroll_x: f64,
    scroll_y: f64,
    visual_offset_left: f64,
    visual_offset_top: f64,
    visual_page_left: f64,
    visual_page_top: f64,
    visual_width: f64,
    visual_height: f64,
    visual_scale: f64,
}

#[derive(Clone, Debug)]
struct FrameViewportSample {
    payload: FrameViewportPayload,
    aperture: Size<FrameViewport>,
    context: MainWorldContext,
}

impl PartialEq for FrameViewportSample {
    fn eq(&self, other: &Self) -> bool {
        self.context.unique_id == other.context.unique_id && self.payload == other.payload
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TopViewportSample {
    aperture: Size<TopViewport>,
    page_x: f64,
    page_y: f64,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
}

#[derive(Clone)]
struct BoundarySample {
    child: LocatorFrameRoute,
    parent: LocatorFrameRoute,
    owner: OwnerSample,
    source: Option<FrameViewportSample>,
}

pub(crate) struct GeometryFence {
    store: Arc<super::FrameStore>,
    routes: Vec<LocatorFrameRoute>,
    boundaries: Vec<BoundarySample>,
    top: Option<(cdpkit::Session, TopViewportSample)>,
}

impl GeometryFence {
    pub(crate) async fn validate(&self, operation: &'static str) -> Result<(), BrowserError> {
        let route_refs = self.routes.iter().collect::<Vec<_>>();
        self.store
            .validate_locator_lineage_authoritative(&route_refs)
            .await?;
        for boundary in &self.boundaries {
            self.store.validate_locator_route(&boundary.child)?;
            self.store.validate_locator_route(&boundary.parent)?;
            let current_owner = read_owner(&boundary.child, &boundary.parent, operation).await?;
            if current_owner != boundary.owner {
                return Err(geometry_error(
                    operation,
                    "frame owner geometry changed during the operation",
                ));
            }
            if let Some(expected) = &boundary.source {
                self.store
                    .validate_main_world_context(&boundary.child, &expected.context)?;
                let current = read_frame_viewport(&self.store, &boundary.child, operation).await?;
                if &current != expected {
                    return Err(geometry_error(
                        operation,
                        "frame viewport or execution context changed during the operation",
                    ));
                }
            }
        }
        if let Some((session, expected)) = &self.top {
            let current = read_top_viewport(session, operation).await?;
            if &current != expected {
                return Err(geometry_error(
                    operation,
                    "top-level visual viewport changed during the operation",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) struct MappedQuad {
    pub(crate) quad: Quad<TopPage>,
    pub(crate) fence: GeometryFence,
}

pub(crate) struct MappedPoint {
    pub(crate) point: Point<SessionViewport>,
    pub(crate) fence: GeometryFence,
}

pub(crate) struct Geometry {
    page: Page,
    store: Arc<super::FrameStore>,
    routes: Vec<LocatorFrameRoute>,
}

impl Geometry {
    pub(crate) async fn for_frame(
        frame: &super::Frame,
        operation: &super::page::PageOperation,
    ) -> Result<Self, BrowserError> {
        let page = frame.page().clone();
        let store = Arc::clone(page.locator_frame_store(operation).await?);
        let route = store.locator_route(frame)?;
        let routes = store.freeze_locator_lineage(&route)?;
        Ok(Self {
            page,
            store,
            routes,
        })
    }

    pub(crate) fn for_route(
        page: &Page,
        store: &Arc<super::FrameStore>,
        route: &LocatorFrameRoute,
    ) -> Result<Self, BrowserError> {
        Ok(Self {
            page: page.clone(),
            store: Arc::clone(store),
            routes: store.freeze_locator_lineage(route)?,
        })
    }

    pub(crate) fn session(&self) -> cdpkit::Session {
        self.routes
            .first()
            .expect("geometry always has a routed frame")
            .session
            .clone()
    }

    pub(crate) fn route_fence(&self) -> GeometryFence {
        GeometryFence {
            store: Arc::clone(&self.store),
            routes: self.routes.clone(),
            boundaries: Vec::new(),
            top: None,
        }
    }

    pub(crate) async fn map_session_quad_to_top_page(
        &self,
        source: Quad<SessionViewport>,
        operation: &'static str,
    ) -> Result<MappedQuad, BrowserError> {
        map_session_quad_to_top_page(&self.page, &self.store, &self.routes, source, operation).await
    }

    pub(crate) async fn map_frame_viewport_to_top_page(
        &self,
        operation: &'static str,
    ) -> Result<MappedQuad, BrowserError> {
        if self.routes.len() == 1 {
            map_top_viewport_to_top_page(&self.page, &self.store, &self.routes, operation).await
        } else {
            map_frame_owner_to_top_page(&self.page, &self.store, &self.routes, operation).await
        }
    }

    pub(crate) async fn map_frame_point_to_session(
        &self,
        point: Point<FrameViewport>,
        operation: &'static str,
    ) -> Result<MappedPoint, BrowserError> {
        map_frame_point_to_session(&self.store, &self.routes, point, operation).await
    }
}

async fn map_top_viewport_to_top_page(
    page: &Page,
    store: &Arc<super::FrameStore>,
    routes: &[LocatorFrameRoute],
    operation: &'static str,
) -> Result<MappedQuad, BrowserError> {
    let top_session = page.cdp_session().clone();
    let top = read_top_viewport(&top_session, operation).await?;
    let quad = Quad::<TopPage>::try_from_slice(
        &[
            top.page_x,
            top.page_y,
            top.page_x + top.aperture.width,
            top.page_y,
            top.page_x + top.aperture.width,
            top.page_y + top.aperture.height,
            top.page_x,
            top.page_y + top.aperture.height,
        ],
        operation,
    )?;
    Ok(MappedQuad {
        quad,
        fence: GeometryFence {
            store: Arc::clone(store),
            routes: routes.to_vec(),
            boundaries: Vec::new(),
            top: Some((top_session, top)),
        },
    })
}

async fn map_frame_owner_to_top_page(
    page: &Page,
    store: &Arc<super::FrameStore>,
    routes: &[LocatorFrameRoute],
    operation: &'static str,
) -> Result<MappedQuad, BrowserError> {
    let child = routes
        .first()
        .ok_or_else(|| geometry_error(operation, "frame route is empty"))?;
    let parent = routes
        .get(1)
        .ok_or_else(|| geometry_error(operation, "embedded frame route has no parent"))?;
    let owner = read_owner(child, parent, operation).await?;
    let mut mapped =
        map_session_quad_to_top_page(page, store, &routes[1..], owner.owner, operation).await?;
    mapped.fence.routes = routes.to_vec();
    mapped.fence.boundaries.insert(
        0,
        BoundarySample {
            child: child.clone(),
            parent: parent.clone(),
            owner,
            source: None,
        },
    );
    Ok(mapped)
}

async fn map_session_quad_to_top_page(
    page: &Page,
    store: &Arc<super::FrameStore>,
    routes: &[LocatorFrameRoute],
    source: Quad<SessionViewport>,
    operation: &'static str,
) -> Result<MappedQuad, BrowserError> {
    let top_session = page.cdp_session().clone();
    let mut mapped = source;
    let mut boundaries = Vec::new();

    for pair in routes.windows(2) {
        let child = &pair[0];
        let parent = &pair[1];
        let owner = read_owner(child, parent, operation).await?;
        if child.session_id == parent.session_id {
            ensure_quad_within_quad(mapped, owner.owner, operation)?;
            boundaries.push(BoundarySample {
                child: child.clone(),
                parent: parent.clone(),
                owner,
                source: None,
            });
            continue;
        }

        let frame_viewport = read_frame_viewport(store, child, operation).await?;
        ensure_source_matches_owner(&frame_viewport, owner.layout, operation)?;
        ensure_quad_within_frame_viewport(mapped, &frame_viewport, operation)?;
        mapped = project_quad(mapped, owner.owner, frame_viewport.aperture, operation)?;
        ensure_quad_within_quad(mapped, owner.owner, operation)?;
        boundaries.push(BoundarySample {
            child: child.clone(),
            parent: parent.clone(),
            owner,
            source: Some(frame_viewport),
        });
    }

    let top = read_top_viewport(&top_session, operation).await?;
    ensure_quad_within_size(mapped, top.aperture, operation)?;
    let mut coordinates = mapped.coordinates;
    for point in coordinates.as_chunks_mut::<2>().0.iter_mut() {
        point[0] += top.page_x;
        point[1] += top.page_y;
    }
    let quad = Quad::<TopPage>::try_from_slice(&coordinates, operation)?;
    Ok(MappedQuad {
        quad,
        fence: GeometryFence {
            store: Arc::clone(store),
            routes: routes.to_vec(),
            boundaries,
            top: Some((top_session, top)),
        },
    })
}

async fn map_frame_point_to_session(
    store: &Arc<super::FrameStore>,
    routes: &[LocatorFrameRoute],
    point: Point<FrameViewport>,
    operation: &'static str,
) -> Result<MappedPoint, BrowserError> {
    let mut quad = Quad::<SessionViewport>::try_from_slice(
        &[
            point.x, point.y, point.x, point.y, point.x, point.y, point.x, point.y,
        ],
        operation,
    )?;
    let mut boundaries = Vec::new();
    for pair in routes.windows(2) {
        let child = &pair[0];
        let parent = &pair[1];
        if child.session_id != parent.session_id {
            break;
        }
        let owner = read_owner(child, parent, operation).await?;
        let frame_viewport = read_frame_viewport(store, child, operation).await?;
        ensure_source_matches_owner(&frame_viewport, owner.layout, operation)?;
        ensure_quad_within_frame_viewport(quad, &frame_viewport, operation)?;
        quad = project_quad(quad, owner.owner, frame_viewport.aperture, operation)?;
        ensure_quad_within_quad(quad, owner.owner, operation)?;
        boundaries.push(BoundarySample {
            child: child.clone(),
            parent: parent.clone(),
            owner,
            source: Some(frame_viewport),
        });
    }
    Ok(MappedPoint {
        point: quad.center(operation)?,
        fence: GeometryFence {
            store: Arc::clone(store),
            routes: routes.to_vec(),
            boundaries,
            top: None,
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OwnerLayoutRect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl OwnerLayoutRect {
    fn size(self, operation: &'static str) -> Result<Size<FrameViewport>, BrowserError> {
        Size::new(self.right - self.left, self.bottom - self.top, operation)
    }

    fn contains(self, other: Self) -> bool {
        (other.left >= self.left || close(other.left, self.left))
            && (other.top >= self.top || close(other.top, self.top))
            && (other.right <= self.right || close(other.right, self.right))
            && (other.bottom <= self.bottom || close(other.bottom, self.bottom))
    }
}

#[derive(Clone, Copy)]
struct Homography {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
    g: f64,
    h: f64,
}

impl Homography {
    fn from_unit_square(
        destination: Quad<SessionViewport>,
        operation: &'static str,
    ) -> Result<Self, BrowserError> {
        validate_convex_quad(destination, "frame owner border box", operation)?;
        let [x0, y0, x1, y1, x2, y2, x3, y3] = destination.coordinates;
        let dx1 = x1 - x2;
        let dx2 = x3 - x2;
        let dx3 = x0 - x1 + x2 - x3;
        let dy1 = y1 - y2;
        let dy2 = y3 - y2;
        let dy3 = y0 - y1 + y2 - y3;
        let (g, h) = if dx3.abs() <= f64::EPSILON && dy3.abs() <= f64::EPSILON {
            (0.0, 0.0)
        } else {
            let determinant = dx1 * dy2 - dx2 * dy1;
            let scale = destination_edge_scale(destination);
            if !determinant.is_finite() || determinant.abs() <= EPSILON * scale * scale {
                return Err(geometry_error(
                    operation,
                    "frame owner border transform is degenerate",
                ));
            }
            (
                (dx3 * dy2 - dx2 * dy3) / determinant,
                (dx1 * dy3 - dx3 * dy1) / determinant,
            )
        };
        let homography = Self {
            a: x1 - x0 + g * x1,
            b: x3 - x0 + h * x3,
            c: x0,
            d: y1 - y0 + g * y1,
            e: y3 - y0 + h * y3,
            f: y0,
            g,
            h,
        };
        let mut denominator_sign = None;
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            let denominator = homography.g * u + homography.h * v + 1.0;
            if !denominator.is_finite() || denominator.abs() <= EPSILON {
                return Err(geometry_error(
                    operation,
                    "frame owner border transform crosses the projection horizon",
                ));
            }
            let sign = denominator.is_sign_positive();
            if denominator_sign.is_some_and(|expected| expected != sign) {
                return Err(geometry_error(
                    operation,
                    "frame owner border transform crosses the projection horizon",
                ));
            }
            denominator_sign = Some(sign);
        }
        Ok(homography)
    }

    fn unproject(
        self,
        x: f64,
        y: f64,
        operation: &'static str,
    ) -> Result<(f64, f64), BrowserError> {
        let aa = self.a - x * self.g;
        let ab = self.b - x * self.h;
        let ba = self.d - y * self.g;
        let bb = self.e - y * self.h;
        let determinant = aa * bb - ab * ba;
        if !determinant.is_finite() || determinant.abs() <= f64::EPSILON {
            return Err(geometry_error(
                operation,
                "frame owner transform crosses the projection horizon",
            ));
        }
        let right_x = x - self.c;
        let right_y = y - self.f;
        let u = (right_x * bb - ab * right_y) / determinant;
        let v = (aa * right_y - right_x * ba) / determinant;
        if !u.is_finite() || !v.is_finite() {
            return Err(geometry_error(
                operation,
                "frame owner inverse projection is non-finite",
            ));
        }
        Ok((u, v))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct OwnerSample {
    owner: Quad<SessionViewport>,
    padding: Quad<SessionViewport>,
    border: Quad<SessionViewport>,
    border_layout: Size<FrameViewport>,
    padding_layout: OwnerLayoutRect,
    content_layout: OwnerLayoutRect,
    layout: Size<FrameViewport>,
}

fn sample_owner_model(
    model: &cdpkit::dom::types::BoxModel,
    operation: &'static str,
) -> Result<OwnerSample, BrowserError> {
    let border_layout = Size::new(model.width as f64, model.height as f64, operation)?;
    let owner = Quad::try_from_slice(&model.content, operation)?;
    let padding = Quad::try_from_slice(&model.padding, operation)?;
    let border = Quad::try_from_slice(&model.border, operation)?;
    let transform = Homography::from_unit_square(border, operation)?;
    let padding_layout = inverse_project_owner_rect(
        padding,
        transform,
        border_layout,
        "frame owner padding box",
        operation,
    )?;
    let content_layout = inverse_project_owner_rect(
        owner,
        transform,
        border_layout,
        "frame owner content box",
        operation,
    )?;
    if !padding_layout.contains(content_layout) {
        return Err(geometry_error(
            operation,
            "frame owner content box is not embedded within its padding box",
        ));
    }
    let layout = content_layout.size(operation)?;
    Ok(OwnerSample {
        owner,
        padding,
        border,
        border_layout,
        padding_layout,
        content_layout,
        layout,
    })
}

fn inverse_project_owner_rect(
    quad: Quad<SessionViewport>,
    transform: Homography,
    border: Size<FrameViewport>,
    name: &'static str,
    operation: &'static str,
) -> Result<OwnerLayoutRect, BrowserError> {
    validate_convex_quad(quad, name, operation)?;
    let mut local = [0.0; 8];
    for (source, destination) in quad
        .coordinates
        .as_chunks::<2>()
        .0
        .iter()
        .zip(local.as_chunks_mut::<2>().0.iter_mut())
    {
        let (u, v) = transform.unproject(source[0], source[1], operation)?;
        destination[0] = u * border.width;
        destination[1] = v * border.height;
    }
    if !close(local[1], local[3])
        || !close(local[2], local[4])
        || !close(local[5], local[7])
        || !close(local[6], local[0])
    {
        return Err(geometry_error(
            operation,
            format!("{name} is not an axis-aligned rectangle in border-local layout space"),
        ));
    }
    let mut rect = OwnerLayoutRect {
        left: (local[0] + local[6]) / 2.0,
        top: (local[1] + local[3]) / 2.0,
        right: (local[2] + local[4]) / 2.0,
        bottom: (local[5] + local[7]) / 2.0,
    };
    if (rect.left < 0.0 && !close(rect.left, 0.0))
        || (rect.top < 0.0 && !close(rect.top, 0.0))
        || (rect.right > border.width && !close(rect.right, border.width))
        || (rect.bottom > border.height && !close(rect.bottom, border.height))
    {
        return Err(geometry_error(
            operation,
            format!("{name} is outside the canonical border box"),
        ));
    }
    rect.left = rect.left.clamp(0.0, border.width);
    rect.top = rect.top.clamp(0.0, border.height);
    rect.right = rect.right.clamp(0.0, border.width);
    rect.bottom = rect.bottom.clamp(0.0, border.height);
    rect.size(operation)?;
    Ok(rect)
}

fn destination_edge_scale(quad: Quad<SessionViewport>) -> f64 {
    let points = quad.coordinates;
    (0..4)
        .flat_map(|index| {
            let next = (index + 1) % 4;
            [
                (points[next * 2] - points[index * 2]).abs(),
                (points[next * 2 + 1] - points[index * 2 + 1]).abs(),
            ]
        })
        .fold(1.0, f64::max)
}

fn validate_convex_quad(
    quad: Quad<SessionViewport>,
    name: &'static str,
    operation: &'static str,
) -> Result<(), BrowserError> {
    let points = quad.coordinates;
    let tolerance = EPSILON * destination_edge_scale(quad).powi(2);
    let mut orientation = None;
    for index in 0..4 {
        let next = (index + 1) % 4;
        let following = (index + 2) % 4;
        let edge_x = points[next * 2] - points[index * 2];
        let edge_y = points[next * 2 + 1] - points[index * 2 + 1];
        let next_edge_x = points[following * 2] - points[next * 2];
        let next_edge_y = points[following * 2 + 1] - points[next * 2 + 1];
        let cross = edge_x * next_edge_y - edge_y * next_edge_x;
        if !cross.is_finite() || cross.abs() <= tolerance {
            return Err(geometry_error(operation, format!("{name} is degenerate")));
        }
        let sign = cross.is_sign_positive();
        if orientation.is_some_and(|expected| expected != sign) {
            return Err(geometry_error(operation, format!("{name} is non-convex")));
        }
        orientation = Some(sign);
    }
    Ok(())
}

async fn read_owner(
    child: &LocatorFrameRoute,
    parent: &LocatorFrameRoute,
    operation: &'static str,
) -> Result<OwnerSample, BrowserError> {
    let owner = GetFrameOwner::new(child.frame_id.as_str().to_owned())
        .send(&parent.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(operation, OperationPhase::Observation, error)
        })?;
    let model = GetBoxModel::new()
        .with_backend_node_id(owner.backend_node_id)
        .send(&parent.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(operation, OperationPhase::Observation, error)
        })?
        .model;
    sample_owner_model(&model, operation)
}

const VIEWPORT_FACT_EXPRESSION: &str = r#"(() => {
  const visual = globalThis.visualViewport;
  return {
    innerWidth: Number(globalThis.innerWidth),
    innerHeight: Number(globalThis.innerHeight),
    scrollX: Number(globalThis.scrollX),
    scrollY: Number(globalThis.scrollY),
    visualOffsetLeft: visual ? Number(visual.offsetLeft) : 0,
    visualOffsetTop: visual ? Number(visual.offsetTop) : 0,
    visualPageLeft: visual ? Number(visual.pageLeft) : Number(globalThis.scrollX),
    visualPageTop: visual ? Number(visual.pageTop) : Number(globalThis.scrollY),
    visualWidth: visual ? Number(visual.width) : Number(globalThis.innerWidth),
    visualHeight: visual ? Number(visual.height) : Number(globalThis.innerHeight),
    visualScale: visual ? Number(visual.scale) : 1
  };
})()"#;

async fn read_frame_viewport(
    store: &super::FrameStore,
    route: &LocatorFrameRoute,
    operation: &'static str,
) -> Result<FrameViewportSample, BrowserError> {
    let context = store.main_world_context(route).await?;
    store.validate_main_world_context(route, &context)?;
    let response = CdpEvaluate::new(VIEWPORT_FACT_EXPRESSION)
        .with_unique_context_id(context.unique_id.clone())
        .with_return_by_value(true)
        .send(&route.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(operation, OperationPhase::Observation, error)
        })?;
    if response.exception_details.is_some() {
        return Err(geometry_error(
            operation,
            "reading frame viewport facts raised a JavaScript exception",
        ));
    }
    let value = response.result.value.ok_or_else(|| {
        geometry_error(operation, "frame viewport facts were not returned by value")
    })?;
    let payload: FrameViewportPayload = serde_json::from_value(value)
        .map_err(|_| geometry_error(operation, "frame viewport facts were malformed"))?;
    let sample = validate_frame_viewport_payload(payload, context, operation)?;
    store.validate_main_world_context(route, &sample.context)?;
    Ok(sample)
}

fn validate_frame_viewport_payload(
    payload: FrameViewportPayload,
    context: MainWorldContext,
    operation: &'static str,
) -> Result<FrameViewportSample, BrowserError> {
    let values = [
        payload.inner_width,
        payload.inner_height,
        payload.scroll_x,
        payload.scroll_y,
        payload.visual_offset_left,
        payload.visual_offset_top,
        payload.visual_page_left,
        payload.visual_page_top,
        payload.visual_width,
        payload.visual_height,
        payload.visual_scale,
    ];
    if !values.into_iter().all(f64::is_finite) {
        return Err(geometry_error(
            operation,
            "frame viewport facts were non-finite",
        ));
    }
    if (payload.visual_scale - 1.0).abs() > SCALE_EPSILON {
        return Err(geometry_error(
            operation,
            "pinch-zoomed frame geometry is unsupported",
        ));
    }
    let aperture = Size::new(payload.inner_width, payload.inner_height, operation)?;
    let visual =
        Size::<FrameViewport>::new(payload.visual_width, payload.visual_height, operation)?;
    if (visual.width > aperture.width && !close(visual.width, aperture.width))
        || (visual.height > aperture.height && !close(visual.height, aperture.height))
    {
        return Err(geometry_error(
            operation,
            "frame visual viewport extends beyond its layout viewport",
        ));
    }
    if payload.visual_offset_left.abs() > EPSILON || payload.visual_offset_top.abs() > EPSILON {
        return Err(geometry_error(
            operation,
            "offset visual viewports are unsupported for routed frame geometry",
        ));
    }
    Ok(FrameViewportSample {
        payload,
        aperture,
        context,
    })
}

async fn read_top_viewport(
    session: &cdpkit::Session,
    operation: &'static str,
) -> Result<TopViewportSample, BrowserError> {
    let metrics = GetLayoutMetrics::new()
        .send(session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(operation, OperationPhase::Observation, error)
        })?;
    let visual = metrics.css_visual_viewport;
    let values = [
        visual.client_width,
        visual.client_height,
        visual.page_x,
        visual.page_y,
        visual.offset_x,
        visual.offset_y,
        visual.scale,
    ];
    if !values.into_iter().all(f64::is_finite) {
        return Err(geometry_error(
            operation,
            "top-level visual viewport is non-finite",
        ));
    }
    if (visual.scale - 1.0).abs() > SCALE_EPSILON {
        return Err(geometry_error(
            operation,
            "pinch-zoomed top-level geometry is unsupported",
        ));
    }
    Ok(TopViewportSample {
        aperture: Size::new(visual.client_width, visual.client_height, operation)?,
        page_x: visual.page_x,
        page_y: visual.page_y,
        offset_x: visual.offset_x,
        offset_y: visual.offset_y,
        scale: visual.scale,
    })
}

fn ensure_source_matches_owner(
    source: &FrameViewportSample,
    owner: Size<FrameViewport>,
    operation: &'static str,
) -> Result<(), BrowserError> {
    if close(source.aperture.width, owner.width) && close(source.aperture.height, owner.height) {
        Ok(())
    } else {
        Err(geometry_error(
            operation,
            "frame viewport dimensions do not match the owner content aperture",
        ))
    }
}

fn ensure_quad_within_frame_viewport(
    quad: Quad<SessionViewport>,
    viewport: &FrameViewportSample,
    operation: &'static str,
) -> Result<(), BrowserError> {
    let left = viewport.payload.visual_offset_left;
    let top = viewport.payload.visual_offset_top;
    let right = left + viewport.payload.visual_width;
    let bottom = top + viewport.payload.visual_height;
    if quad.coordinates.as_chunks::<2>().0.iter().all(|point| {
        point[0] >= left - EPSILON
            && point[1] >= top - EPSILON
            && point[0] <= right + EPSILON
            && point[1] <= bottom + EPSILON
    }) {
        Ok(())
    } else {
        Err(geometry_error(
            operation,
            "geometry quad is outside the frame visual aperture",
        ))
    }
}

fn ensure_quad_within_size<Space>(
    quad: Quad<Space>,
    size: Size<TopViewport>,
    operation: &'static str,
) -> Result<(), BrowserError> {
    if quad.coordinates.as_chunks::<2>().0.iter().all(|point| {
        point[0] >= -EPSILON
            && point[1] >= -EPSILON
            && point[0] <= size.width + EPSILON
            && point[1] <= size.height + EPSILON
    }) {
        Ok(())
    } else {
        Err(geometry_error(
            operation,
            "geometry quad is outside the top-level visual aperture",
        ))
    }
}

fn ensure_quad_within_quad(
    quad: Quad<SessionViewport>,
    aperture: Quad<SessionViewport>,
    operation: &'static str,
) -> Result<(), BrowserError> {
    let points = aperture
        .coordinates
        .as_chunks::<2>()
        .0
        .iter()
        .map(|point| (point[0], point[1]))
        .collect::<Vec<_>>();
    let mut orientation = 0.0_f64;
    for index in 0..4 {
        let current = points[index];
        let next = points[(index + 1) % 4];
        let following = points[(index + 2) % 4];
        let cross = (next.0 - current.0) * (following.1 - next.1)
            - (next.1 - current.1) * (following.0 - next.0);
        if cross.abs() > EPSILON {
            if orientation != 0.0 && orientation.signum() != cross.signum() {
                return Err(geometry_error(
                    operation,
                    "frame owner content aperture is non-convex",
                ));
            }
            orientation = cross;
        }
    }
    if orientation == 0.0 {
        return Err(geometry_error(
            operation,
            "frame owner content aperture is degenerate",
        ));
    }
    if quad.coordinates.as_chunks::<2>().0.iter().all(|point| {
        (0..4).all(|index| {
            let start = points[index];
            let end = points[(index + 1) % 4];
            let cross =
                (end.0 - start.0) * (point[1] - start.1) - (end.1 - start.1) * (point[0] - start.0);
            cross * orientation >= -EPSILON
        })
    }) {
        Ok(())
    } else {
        Err(geometry_error(
            operation,
            "geometry quad is outside its frame owner content aperture",
        ))
    }
}

fn project_quad(
    quad: Quad<SessionViewport>,
    destination: Quad<SessionViewport>,
    source: Size<FrameViewport>,
    operation: &'static str,
) -> Result<Quad<SessionViewport>, BrowserError> {
    let [x0, y0, x1, y1, x2, y2, x3, y3] = destination.coordinates;
    let dx1 = x1 - x2;
    let dx2 = x3 - x2;
    let dx3 = x0 - x1 + x2 - x3;
    let dy1 = y1 - y2;
    let dy2 = y3 - y2;
    let dy3 = y0 - y1 + y2 - y3;
    let (g, h) = if dx3.abs() <= f64::EPSILON && dy3.abs() <= f64::EPSILON {
        (0.0, 0.0)
    } else {
        let determinant = dx1 * dy2 - dx2 * dy1;
        if !determinant.is_finite() || determinant.abs() <= f64::EPSILON {
            return Err(geometry_error(
                operation,
                "frame owner transform is degenerate",
            ));
        }
        (
            (dx3 * dy2 - dx2 * dy3) / determinant,
            (dx1 * dy3 - dx3 * dy1) / determinant,
        )
    };
    let a = x1 - x0 + g * x1;
    let b = x3 - x0 + h * x3;
    let c = x0;
    let d = y1 - y0 + g * y1;
    let e = y3 - y0 + h * y3;
    let f = y0;
    let mut projected = [0.0; 8];
    let mut denominator_sign = None;
    for (input, output) in quad
        .coordinates
        .as_chunks::<2>()
        .0
        .iter()
        .zip(projected.as_chunks_mut::<2>().0.iter_mut())
    {
        let u = input[0] / source.width;
        let v = input[1] / source.height;
        let denominator = g * u + h * v + 1.0;
        if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
            return Err(geometry_error(
                operation,
                "frame owner transform crosses the projection horizon",
            ));
        }
        let sign = denominator.is_sign_positive();
        if denominator_sign.is_some_and(|expected| expected != sign) {
            return Err(geometry_error(
                operation,
                "frame owner transform crosses the projection horizon",
            ));
        }
        denominator_sign = Some(sign);
        output[0] = (a * u + b * v + c) / denominator;
        output[1] = (d * u + e * v + f) / denominator;
    }
    Quad::try_from_slice(&projected, operation)
}

pub(crate) fn ensure_axis_aligned<Space>(
    quad: Quad<Space>,
    operation: &'static str,
) -> Result<(), BrowserError> {
    let coordinates = quad.coordinates;
    if close(coordinates[1], coordinates[3])
        && close(coordinates[2], coordinates[4])
        && close(coordinates[5], coordinates[7])
        && close(coordinates[6], coordinates[0])
    {
        Ok(())
    } else {
        Err(geometry_error(
            operation,
            "geometry quad cannot be represented by an axis-aligned clip",
        ))
    }
}

fn close(left: f64, right: f64) -> bool {
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= SCALE_EPSILON * scale
}

fn geometry_error(operation: &'static str, message: impl Into<String>) -> BrowserError {
    BrowserError::operation(operation, OperationPhase::Observation).with_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_quad(values: [f64; 8]) -> Quad<SessionViewport> {
        Quad::try_from_slice(&values, "test geometry").unwrap()
    }

    fn viewport_payload(width: f64, height: f64) -> FrameViewportPayload {
        viewport_payload_with_visual(width, height, width, height)
    }

    fn viewport_payload_with_visual(
        inner_width: f64,
        inner_height: f64,
        visual_width: f64,
        visual_height: f64,
    ) -> FrameViewportPayload {
        FrameViewportPayload {
            inner_width,
            inner_height,
            scroll_x: 13.0,
            scroll_y: 17.0,
            visual_offset_left: 0.0,
            visual_offset_top: 0.0,
            visual_page_left: 13.0,
            visual_page_top: 17.0,
            visual_width,
            visual_height,
            visual_scale: 1.0,
        }
    }

    fn context(unique_id: &str) -> MainWorldContext {
        MainWorldContext {
            id: 1,
            unique_id: unique_id.to_owned(),
        }
    }

    fn fake_box_model(
        content: [f64; 8],
        padding: [f64; 8],
        border: [f64; 8],
        width: i64,
        height: i64,
    ) -> cdpkit::dom::types::BoxModel {
        serde_json::from_value(serde_json::json!({
            "content": content,
            "padding": padding,
            "border": border,
            "margin": border,
            "width": width,
            "height": height
        }))
        .expect("fake CDP BoxModel")
    }

    fn axis_aligned_quad(left: f64, top: f64, right: f64, bottom: f64) -> [f64; 8] {
        [left, top, right, top, right, bottom, left, bottom]
    }

    #[test]
    fn owner_model_uses_border_box_only_as_canonical_space() {
        for model in [
            // CSS content-box: 300x150 content plus a symmetric 2px border.
            fake_box_model(
                axis_aligned_quad(12.0, 22.0, 312.0, 172.0),
                axis_aligned_quad(12.0, 22.0, 312.0, 172.0),
                axis_aligned_quad(10.0, 20.0, 314.0, 174.0),
                304,
                154,
            ),
            // CSS border-box: the same canonical CDP border/content geometry.
            fake_box_model(
                axis_aligned_quad(12.0, 22.0, 312.0, 172.0),
                axis_aligned_quad(12.0, 22.0, 312.0, 172.0),
                axis_aligned_quad(10.0, 20.0, 314.0, 174.0),
                304,
                154,
            ),
        ] {
            let sample = sample_owner_model(&model, "test geometry").unwrap();
            assert_eq!(
                (sample.border_layout.width, sample.border_layout.height),
                (304.0, 154.0)
            );
            assert_eq!((sample.layout.width, sample.layout.height), (300.0, 150.0));
        }
    }

    #[test]
    fn owner_model_recovers_asymmetric_border_and_padding_content_aperture() {
        let model = fake_box_model(
            axis_aligned_quad(114.0, 218.0, 416.0, 390.0),
            axis_aligned_quad(103.0, 205.0, 433.0, 411.0),
            axis_aligned_quad(100.0, 200.0, 440.0, 420.0),
            340,
            220,
        );
        let sample = sample_owner_model(&model, "test geometry").unwrap();
        assert_eq!((sample.layout.width, sample.layout.height), (302.0, 172.0));
    }

    #[test]
    fn owner_model_supports_scale_rotation_and_bounded_perspective() {
        let border_size = Size::<FrameViewport>::new(340.0, 220.0, "test geometry").unwrap();
        let content_local = session_quad(axis_aligned_quad(14.0, 18.0, 316.0, 190.0));
        let padding_local = session_quad(axis_aligned_quad(3.0, 5.0, 333.0, 211.0));
        for destination in [
            session_quad([20.0, 30.0, 680.0, 30.0, 680.0, 470.0, 20.0, 470.0]),
            session_quad([70.0, 20.0, 390.0, 130.0, 320.0, 340.0, 0.0, 230.0]),
            session_quad([20.0, 30.0, 390.0, 70.0, 330.0, 280.0, -10.0, 220.0]),
        ] {
            let content = project_quad(content_local, destination, border_size, "test geometry")
                .unwrap()
                .coordinates();
            let padding = project_quad(padding_local, destination, border_size, "test geometry")
                .unwrap()
                .coordinates();
            let model = fake_box_model(content, padding, destination.coordinates(), 340, 220);
            let sample = sample_owner_model(&model, "test geometry").unwrap();
            assert!(close(sample.layout.width, 302.0));
            assert!(close(sample.layout.height, 172.0));
        }
    }

    #[test]
    fn owner_model_invalid_border_geometry_fails_closed() {
        let content = axis_aligned_quad(1.0, 1.0, 9.0, 9.0);
        for border in [[0.0; 8], [0.0, 0.0, 10.0, 0.0, 0.0, 10.0, 10.0, 10.0]] {
            let model = fake_box_model(content, content, border, 10, 10);
            assert!(sample_owner_model(&model, "test geometry").is_err());
        }
    }

    #[test]
    fn owner_sample_keeps_border_padding_and_transform_fence_facts() {
        let baseline = fake_box_model(
            axis_aligned_quad(2.0, 2.0, 302.0, 152.0),
            axis_aligned_quad(2.0, 2.0, 302.0, 152.0),
            axis_aligned_quad(0.0, 0.0, 304.0, 154.0),
            304,
            154,
        );
        let changed_padding = fake_box_model(
            axis_aligned_quad(2.0, 2.0, 302.0, 152.0),
            axis_aligned_quad(1.0, 1.0, 303.0, 153.0),
            axis_aligned_quad(0.0, 0.0, 304.0, 154.0),
            304,
            154,
        );
        let changed_border = fake_box_model(
            axis_aligned_quad(2.0, 2.0, 302.0, 152.0),
            axis_aligned_quad(2.0, 2.0, 302.0, 152.0),
            axis_aligned_quad(0.0, 0.0, 306.0, 154.0),
            306,
            154,
        );
        let changed_transform = fake_box_model(
            axis_aligned_quad(12.0, 12.0, 312.0, 162.0),
            axis_aligned_quad(12.0, 12.0, 312.0, 162.0),
            axis_aligned_quad(10.0, 10.0, 314.0, 164.0),
            304,
            154,
        );
        let baseline = sample_owner_model(&baseline, "test geometry").unwrap();
        assert_ne!(
            baseline,
            sample_owner_model(&changed_padding, "test geometry").unwrap()
        );
        assert_ne!(
            baseline,
            sample_owner_model(&changed_border, "test geometry").unwrap()
        );
        assert_ne!(
            baseline,
            sample_owner_model(&changed_transform, "test geometry").unwrap()
        );
    }

    #[test]
    fn owner_content_aperture_maps_same_process_point_and_accepts_oopif_viewport() {
        let model = fake_box_model(
            axis_aligned_quad(12.0, 22.0, 312.0, 172.0),
            axis_aligned_quad(12.0, 22.0, 312.0, 172.0),
            axis_aligned_quad(10.0, 20.0, 314.0, 174.0),
            304,
            154,
        );
        let sample = sample_owner_model(&model, "test geometry").unwrap();
        let mapped = project_quad(
            session_quad([15.0, 15.0, 15.0, 15.0, 15.0, 15.0, 15.0, 15.0]),
            sample.owner,
            sample.layout,
            "test geometry",
        )
        .unwrap()
        .center("test geometry")
        .unwrap();
        assert_eq!((mapped.x(), mapped.y()), (27.0, 37.0));

        let oopif = validate_frame_viewport_payload(
            viewport_payload(300.0, 150.0),
            context("oopif-context"),
            "test geometry",
        )
        .unwrap();
        ensure_source_matches_owner(&oopif, sample.layout, "test geometry").unwrap();
    }

    #[test]
    fn cross_session_projection_uses_frame_context_aperture_not_target_layout_metrics() {
        let source = Size::<FrameViewport>::new(80.0, 60.0, "test geometry").unwrap();
        let destination = session_quad([200.0, 20.0, 280.0, 20.0, 280.0, 80.0, 200.0, 80.0]);
        let element = session_quad([5.0, 6.0, 39.0, 6.0, 39.0, 28.0, 5.0, 28.0]);
        let mapped = project_quad(element, destination, source, "test geometry").unwrap();
        assert_eq!(
            mapped.coordinates(),
            [205.0, 26.0, 239.0, 26.0, 239.0, 48.0, 205.0, 48.0]
        );
        let bounds = mapped.bounds("test geometry").unwrap();
        assert_eq!((bounds.width, bounds.height), (34.0, 22.0));
    }

    #[test]
    fn homography_preserves_supported_non_axis_aligned_owner_transform() {
        let source = Size::<FrameViewport>::new(100.0, 50.0, "test geometry").unwrap();
        let destination = session_quad([10.0, 20.0, 110.0, 40.0, 90.0, 90.0, -10.0, 70.0]);
        let mapped = project_quad(
            session_quad([0.0, 0.0, 100.0, 0.0, 100.0, 50.0, 0.0, 50.0]),
            destination,
            source,
            "test geometry",
        )
        .unwrap();
        assert_eq!(mapped, destination);
        assert!(ensure_axis_aligned(mapped, "test geometry").is_err());
    }

    #[test]
    fn invalid_quads_and_projection_horizons_fail_closed() {
        assert!(ensure_quad_within_quad(
            session_quad([1.0; 8]),
            session_quad([0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0]),
            "test geometry",
        )
        .is_err());
        assert!(Quad::<SessionViewport>::try_from_slice(
            &[0.0, 0.0, f64::NAN, 0.0, 1.0, 1.0, 0.0, 1.0],
            "test geometry",
        )
        .is_err());
    }

    #[test]
    fn frame_scroll_facts_are_sampled_without_changing_css_size() {
        let sample = validate_frame_viewport_payload(
            viewport_payload(80.0, 60.0),
            context("context-a"),
            "test geometry",
        )
        .unwrap();
        assert_eq!(sample.aperture.width, 80.0);
        assert_eq!(sample.aperture.height, 60.0);
        assert_eq!(sample.payload.scroll_x, 13.0);
        assert_eq!(sample.payload.scroll_y, 17.0);
    }

    #[test]
    fn scrollbar_reduced_visual_apertures_preserve_layout_projection() {
        let destination = session_quad([200.0, 20.0, 280.0, 20.0, 280.0, 80.0, 200.0, 80.0]);
        let element = session_quad([9.0, 19.0, 43.0, 19.0, 43.0, 41.0, 9.0, 41.0]);

        for (visual_width, visual_height) in
            [(65.0, 60.0), (80.0, 45.0), (65.0, 45.0), (80.0, 60.0)]
        {
            let sample = validate_frame_viewport_payload(
                viewport_payload_with_visual(80.0, 60.0, visual_width, visual_height),
                context("context-a"),
                "test geometry",
            )
            .unwrap();
            assert_eq!(
                (sample.aperture.width, sample.aperture.height),
                (80.0, 60.0)
            );
            ensure_source_matches_owner(
                &sample,
                Size::new(80.0, 60.0, "test geometry").unwrap(),
                "test geometry",
            )
            .unwrap();
            ensure_quad_within_frame_viewport(element, &sample, "test geometry").unwrap();

            let mapped =
                project_quad(element, destination, sample.aperture, "test geometry").unwrap();
            let bounds = mapped.bounds("test geometry").unwrap();
            assert_eq!((bounds.width, bounds.height), (34.0, 22.0));
        }
    }

    #[test]
    fn invalid_frame_visual_apertures_fail_closed() {
        let invalid_dimensions = [
            (80.0 + 1e-3, 60.0),
            (80.0, 60.0 + 1e-3),
            (0.0, 60.0),
            (65.0, -1.0),
            (f64::NAN, 60.0),
            (65.0, f64::INFINITY),
        ];
        for (visual_width, visual_height) in invalid_dimensions {
            assert!(validate_frame_viewport_payload(
                viewport_payload_with_visual(80.0, 60.0, visual_width, visual_height),
                context("context-a"),
                "test geometry",
            )
            .is_err());
        }

        let mut pinch = viewport_payload_with_visual(80.0, 60.0, 65.0, 60.0);
        pinch.visual_scale = 1.25;
        assert!(
            validate_frame_viewport_payload(pinch, context("context-a"), "test geometry").is_err()
        );

        let mut offset = viewport_payload_with_visual(80.0, 60.0, 65.0, 60.0);
        offset.visual_offset_left = 1.0;
        assert!(
            validate_frame_viewport_payload(offset, context("context-a"), "test geometry").is_err()
        );
    }

    #[test]
    fn visual_aperture_limits_visibility_without_changing_layout_aperture() {
        let sample = validate_frame_viewport_payload(
            viewport_payload_with_visual(80.0, 60.0, 65.0, 45.0),
            context("context-a"),
            "test geometry",
        )
        .unwrap();
        let outside_visual = session_quad([60.0, 40.0, 70.0, 40.0, 70.0, 50.0, 60.0, 50.0]);
        assert!(
            ensure_quad_within_frame_viewport(outside_visual, &sample, "test geometry",).is_err()
        );
        assert_eq!(
            (sample.aperture.width, sample.aperture.height),
            (80.0, 60.0)
        );
    }

    #[test]
    fn scroll_and_visual_page_position_remain_geometry_fence_facts() {
        let sample = validate_frame_viewport_payload(
            viewport_payload_with_visual(80.0, 60.0, 65.0, 60.0),
            context("context-a"),
            "test geometry",
        )
        .unwrap();

        let mut changed_scroll = sample.payload.clone();
        changed_scroll.scroll_y += 1.0;
        let changed_scroll =
            validate_frame_viewport_payload(changed_scroll, context("context-a"), "test geometry")
                .unwrap();
        assert_ne!(sample, changed_scroll);

        let mut changed_visual_page = sample.payload.clone();
        changed_visual_page.visual_page_left += 1.0;
        let changed_visual_page = validate_frame_viewport_payload(
            changed_visual_page,
            context("context-a"),
            "test geometry",
        )
        .unwrap();
        assert_ne!(sample, changed_visual_page);
    }

    #[test]
    fn pinch_offset_source_mismatch_and_context_change_fail_closed() {
        let mut pinch = viewport_payload(80.0, 60.0);
        pinch.visual_scale = 1.25;
        assert!(
            validate_frame_viewport_payload(pinch, context("context-a"), "test geometry")
                .unwrap_err()
                .to_string()
                .contains("pinch")
        );

        let sample = validate_frame_viewport_payload(
            viewport_payload(80.0, 60.0),
            context("context-a"),
            "test geometry",
        )
        .unwrap();
        assert!(ensure_source_matches_owner(
            &sample,
            Size::new(800.0, 600.0, "test geometry").unwrap(),
            "test geometry",
        )
        .is_err());
        let replaced = validate_frame_viewport_payload(
            viewport_payload(80.0, 60.0),
            context("context-b"),
            "test geometry",
        )
        .unwrap();
        assert_ne!(sample, replaced);
    }

    #[test]
    fn projection_horizon_fails_closed() {
        let source = Size::<FrameViewport>::new(1.0, 1.0, "test geometry").unwrap();
        let destination = session_quad([0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
        let crossing = session_quad([0.0, 0.25, 1.0, 0.25, 1.0, 0.75, 0.0, 0.75]);
        assert!(project_quad(crossing, destination, source, "test geometry")
            .unwrap_err()
            .to_string()
            .contains("horizon"));
    }

    #[test]
    fn top_page_conversion_is_css_pixels_and_never_applies_dpr() {
        let viewport = session_quad([1.0, 2.0, 35.0, 2.0, 35.0, 24.0, 1.0, 24.0]);
        let mut coordinates = viewport.coordinates();
        for point in coordinates.as_chunks_mut::<2>().0.iter_mut() {
            point[0] += 7.0;
            point[1] += 11.0;
        }
        let page = Quad::<TopPage>::try_from_slice(&coordinates, "test geometry").unwrap();
        assert_eq!(
            page.bounds("test geometry").unwrap(),
            Bounds::new(8.0, 13.0, 34.0, 22.0, "test geometry").unwrap()
        );
    }
}
