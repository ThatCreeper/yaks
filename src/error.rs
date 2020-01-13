pub use hecs::{ComponentError, MissingComponent, NoSuchEntity};
pub use resources::{CantGetResource as ResourceError, NoSuchResource};
use std::{
    error::Error,
    fmt::{Display, Formatter, Result as FmtResult},
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NoSuchSystem;

impl Display for NoSuchSystem {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.pad("no such system")
    }
}

impl Error for NoSuchSystem {}

#[derive(Debug, Clone, Eq, PartialEq)]
struct CyclicDependency;

impl Display for CyclicDependency {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.pad("adding the system would create an unresolvable cycle")
    }
}

impl Error for CyclicDependency {}
