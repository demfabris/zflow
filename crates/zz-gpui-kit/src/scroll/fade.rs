use std::rc::Rc;

use zz_gpui::{
    Bounds, Hsla, IntoElement, Pixels, Styled, canvas, fill, linear_color_stop, linear_gradient,
    point, size,
};

use super::ScrollbarHandle;

const SOLID_FROM: f32 = 0.7;
const REACH: f32 = 2.0;

/// A fade over the bottom edge of a scroll area, toward `ground`, while there
/// is content below. Its last 30% is solid so clipped content never shows a
/// cut edge, and it is never taller than twice what is left to scroll, so it
/// shrinks away as the end arrives. It takes no pointer events.
pub fn bottom_fade<H: ScrollbarHandle>(
    handle: Rc<H>,
    ground: Hsla,
    height: Pixels,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, _| {
            let below = handle.content_size().height - bounds.size.height + handle.offset().y;
            let height = height.min(below * REACH);
            if height <= Pixels::ZERO {
                return;
            }
            let strip = Bounds::new(
                point(bounds.left(), bounds.bottom() - height),
                size(bounds.size.width, height),
            );
            window.paint_quad(fill(
                strip,
                linear_gradient(
                    180.0,
                    linear_color_stop(ground.alpha(0.0), 0.0),
                    linear_color_stop(ground, SOLID_FROM),
                ),
            ));
        },
    )
    .absolute()
    .inset_0()
}
