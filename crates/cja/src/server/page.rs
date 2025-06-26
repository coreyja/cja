mod factory;
pub use factory::Factory;
use maud::Render;

pub struct Page {
    #[allow(dead_code)]
    content: Box<dyn Render>,
}
