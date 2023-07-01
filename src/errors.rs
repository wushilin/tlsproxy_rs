#[derive(Debug)]
pub struct PipeError {
    msg: String,
}

impl std::error::Error for PipeError {}

impl std::fmt::Display for PipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl PipeError {
    pub fn wrap(msg: String) -> PipeError {
        return PipeError{msg};
    }

    pub fn wrap_box(msg:String) -> Box<PipeError>{
        return Box::new(Self::wrap(msg));
    }
}