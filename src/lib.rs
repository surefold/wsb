//! # watershed-build
//!
//! A typed, deterministic build file language whose values live in a
//! watershed graph. Edit the file and only the values whose formula changed,
//! or that use one that did, are worked out again; ask about any value and
//! it explains where it came from.
//!
//! ```
//! use watershed_build::Workspace;
//!
//! let src = r#"
//!     val ci = env("CI") == "true"
//!
//!     module core {
//!         jvmTarget = if (ci) 21 else 17
//!     }
//!
//!     module app {
//!         dependsOn(core)
//!         jvmTarget = core.jvmTarget
//!         dependency("com.fasterxml.jackson.core:jackson-databind:2.17.1")
//!     }
//! "#;
//! let ci = |var: &str| (var == "CI").then(|| "true".to_string());
//! let w = Workspace::new(src, &ci).unwrap();
//! assert_eq!(w.value("app.jvmTarget").unwrap().to_string(), "21");
//! assert_eq!(w.value("app.kotlin").unwrap().to_string(), "\"2.4.21\""); // the default
//! println!("{}", w.explain("app.jvmTarget").unwrap());
//! ```
//!
//! A typo is an error, not a setting that is quietly ignored:
//!
//! ```
//! use watershed_build::{Workspace, process_env};
//!
//! let err = Workspace::new("module app { jvmTaget = 21 }", &process_env).err().unwrap();
//! assert_eq!(err.message, "there is no setting `jvmTaget`");
//! assert_eq!(err.help.as_deref(), Some("did you mean `jvmTarget`?"));
//! ```

#![forbid(unsafe_code)]

pub mod abi;
pub mod build;
pub mod daemon;
pub mod diagnostic;
pub mod lsp;
pub mod project;
pub mod syntax;
pub mod workspace;

pub use diagnostic::Diagnostic;
pub use project::{Binding, Kind, ModuleInfo, Project, SETTINGS};
pub use workspace::{Change, Env, Workspace, process_env};
