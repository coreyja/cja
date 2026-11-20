use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub struct Rejection(Box<Response>);

impl From<Response> for Rejection {
    fn from(value: Response) -> Self {
        Self(Box::new(value))
    }
}

impl From<StatusCode> for Rejection {
    fn from(value: StatusCode) -> Self {
        Self(Box::new(value.into_response()))
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        *self.0
    }
}
