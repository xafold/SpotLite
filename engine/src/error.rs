/// An error as text. `fatal` ones stop an indexing run (the model failed, the index was built with other settings),
/// pending files stay pending; the rest are a file's own problem, noted on it.
#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub fatal: bool,
}

impl Error {
    pub fn fatal(message: impl Into<String>) -> Error {
        Error { message: message.into(), fatal: true }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for Error {
    fn from(message: String) -> Error {
        Error { message, fatal: false }
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Error {
        message.to_string().into()
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Error {
        error.to_string().into()
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Error {
        error.to_string().into()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
