use std::fmt;

const DEFAULT_CONFIG_PATH: &str = "tape-sync.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    pub config_path: String,
    pub show_help: bool,
}

impl CliArgs {
    pub fn parse_from<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut iter = args.into_iter();
        let program_name = iter.next().unwrap_or_else(|| "tape-sync-tk".to_string());
        let mut config_path = DEFAULT_CONFIG_PATH.to_string();
        let mut positional_path = None;

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    return Ok(Self {
                        config_path,
                        show_help: true,
                    });
                }
                "-c" | "--config" => {
                    let value = iter.next().ok_or(CliError::MissingConfigPath)?;
                    config_path = value;
                }
                _ if arg.starts_with('-') => {
                    return Err(CliError::UnknownFlag(arg));
                }
                _ => {
                    if positional_path.replace(arg).is_some() {
                        return Err(CliError::UnexpectedArgument);
                    }
                }
            }
        }

        if let Some(path) = positional_path {
            config_path = path;
        }

        if config_path.trim().is_empty() {
            return Err(CliError::MissingConfigPath);
        }

        let _ = program_name;

        Ok(Self {
            config_path,
            show_help: false,
        })
    }

    pub fn help_text(program_name: &str) -> String {
        format!(
            "Usage: {program_name} [--config <path>] [config-path]\n\nOptions:\n  -c, --config <path>  Path to TOML config file\n  -h, --help           Show this help text\n\nDefaults:\n  Config path defaults to {DEFAULT_CONFIG_PATH}\n"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingConfigPath,
    UnknownFlag(String),
    UnexpectedArgument,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConfigPath => f.write_str("missing value for --config"),
            Self::UnknownFlag(flag) => write!(f, "unknown argument: {flag}"),
            Self::UnexpectedArgument => f.write_str("too many positional arguments"),
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn defaults_to_standard_config_path() {
        let parsed = CliArgs::parse_from(args(&["tape-sync-tk"]))
            .expect("default parsing should succeed");

        assert_eq!(parsed.config_path, "tape-sync.toml");
        assert!(!parsed.show_help);
    }

    #[test]
    fn accepts_explicit_config_flag() {
        let parsed = CliArgs::parse_from(args(&["tape-sync-tk", "--config", "custom.toml"]))
            .expect("flag parsing should succeed");

        assert_eq!(parsed.config_path, "custom.toml");
    }

    #[test]
    fn accepts_help_flag() {
        let parsed = CliArgs::parse_from(args(&["tape-sync-tk", "--help"]))
            .expect("help parsing should succeed");

        assert!(parsed.show_help);
    }

    #[test]
    fn rejects_unknown_flag() {
        let error = CliArgs::parse_from(args(&["tape-sync-tk", "--verbose"]))
            .expect_err("unknown flag should fail");

        assert_eq!(error, CliError::UnknownFlag("--verbose".to_string()));
    }
}
