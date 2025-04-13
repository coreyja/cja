mod factory;
pub use factory::Factory;
use maud::Render;

struct Page {
    content: Box<dyn Render>,
}
