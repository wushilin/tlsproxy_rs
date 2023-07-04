#[derive(Debug)]
pub struct GeneralError {
    msg: String,
}

impl std::error::Error for GeneralError {}

impl std::fmt::Display for GeneralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl GeneralError {
    pub fn wrap(msg: String) -> GeneralError {
        return GeneralError{msg};
    }

    pub fn wrap_box(msg:String) -> Box<GeneralError>{
        return Box::new(Self::wrap(msg));
    }
}