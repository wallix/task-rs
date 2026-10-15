//! The path to the value being deserialized, like `tasks.build.cmds[0]`, used
//! to prefix error messages.

use std::fmt::{self, Display};

#[derive(Copy, Clone)]
pub(crate) enum Path<'a> {
    Root,
    Seq { parent: &'a Path<'a>, index: usize },
    Map { parent: &'a Path<'a>, key: &'a str },
    Alias { parent: &'a Path<'a> },
    Unknown { parent: &'a Path<'a> },
}

impl Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Parent<'a>(&'a Path<'a>);

        impl Display for Parent<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    Path::Root => Ok(()),
                    path => write!(f, "{path}."),
                }
            }
        }

        match self {
            Path::Root => f.write_str("."),
            Path::Seq { parent, index } => write!(f, "{parent}[{index}]"),
            Path::Map { parent, key } => write!(f, "{}{key}", Parent(parent)),
            Path::Alias { parent } => write!(f, "{parent}"),
            Path::Unknown { parent } => write!(f, "{}?", Parent(parent)),
        }
    }
}
