//! The command line: what every runner takes, and `--gpu-adapter`.

use std::ffi::OsString;

use anyhow::Result;
use lexopt::ValueExt as _;
use wado_host::runner::{Invocation, RunOptions, usage};

/// What `--help` prints ahead of the adapter list.
pub fn help() -> String {
    usage(
        "run-webgpu",
        "Run a Wado program on a wasi:webgpu host.",
        "      \
      --gpu-adapter <index|name>
                         Give the program one adapter, whatever it requests:
                         an integer is its index in the adapter list, anything
                         else a case-insensitive part of its name.
                         --log-level info names each adapter it returns
",
    )
}

/// How `--gpu-adapter` names the one adapter every `request-adapter` returns.
#[derive(PartialEq, Eq, Debug)]
pub enum AdapterSelector {
    /// The position in the order the adapters are enumerated and listed.
    Index(usize),
    /// A case-insensitive substring of the adapter's name.
    Name(String),
}

impl AdapterSelector {
    fn parse(value: String) -> Self {
        value.parse().map_or(Self::Name(value), Self::Index)
    }
}

/// What the command line asks for, and the adapter it pins.
pub fn parse<I>(argv: I) -> Result<(Invocation, Option<AdapterSelector>)>
where
    I: IntoIterator<Item = OsString>,
{
    let mut gpu_adapter = None;
    let invocation = RunOptions::parse(argv, &help(), |name, parser| {
        if name != "gpu-adapter" {
            return Ok(false);
        }
        gpu_adapter = Some(AdapterSelector::parse(parser.value()?.string()?));
        Ok(true)
    })?;
    Ok((invocation, gpu_adapter))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(argv: &[&str]) -> Result<Option<AdapterSelector>> {
        Ok(parse(argv.iter().map(OsString::from))?.1)
    }

    #[test]
    fn a_gpu_adapter_is_an_index_when_an_integer_and_a_name_otherwise() {
        let pinned = |value: &str| adapter(&["--gpu-adapter", value, "app.wado"]).unwrap();
        assert_eq!(adapter(&["app.wado"]).unwrap(), None);
        assert_eq!(pinned("1"), Some(AdapterSelector::Index(1)));
        assert_eq!(
            pinned("nvidia"),
            Some(AdapterSelector::Name("nvidia".to_owned()))
        );
        assert_eq!(pinned("-1"), Some(AdapterSelector::Name("-1".to_owned())));
        assert!(adapter(&["--gpu-adapter"]).is_err());
    }
}
