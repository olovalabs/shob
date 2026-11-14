mod field;
mod form;

pub use field::*;
pub use form::*;

pub fn v_form() -> Form {
    Form::vertical()
}

pub fn h_form() -> Form {
    Form::horizontal()
}

pub fn field() -> Field {
    Field::new()
}
