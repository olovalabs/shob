mod band;
mod linear;
mod ordinal;
mod point;
mod sealed;

pub use band::ScaleBand;
pub use linear::ScaleLinear;
pub use ordinal::ScaleOrdinal;
pub use point::ScalePoint;
pub(crate) use sealed::Sealed;

pub trait Scale<T> {

    fn tick(&self, value: &T) -> Option<f32>;

    fn least_index(&self, _tick: f32) -> usize {
        0
    }

    fn least_index_with_domain(&self, _tick: f32, _domain: &[T]) -> (usize, f32) {
        (0, 0.)
    }
}
