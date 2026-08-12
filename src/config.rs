use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,
    pub api_token: String,
    pub libraries: Vec<LibraryConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LibraryConfig {
    pub library_path: PathBuf,
    pub port: u16,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

/// Fully resolved configuration for a single library's server instance:
/// the global host/token plus that library's own path and port.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub api_token: String,
    pub library_path: PathBuf,
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(".config")
            .join("phoserv")
            .join("config.toml")
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path();
        let contents = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read config file at {}: {e}\n\n\
                 Create it with contents like:\n\n\
                 host = \"127.0.0.1\"\n\
                 api_token = \"<some-secret-token>\"\n\n\
                 [[libraries]]\n\
                 library_path = \"/path/to/test-library\"\n\
                 port = 4173\n\n\
                 [[libraries]]\n\
                 library_path = \"/path/to/other-library\"\n\
                 port = 4174\n",
                path.display()
            )
        })?;
        let mut config: Config = toml::from_str(&contents)?;

        if config.libraries.is_empty() {
            anyhow::bail!("config must define at least one [[libraries]] entry");
        }

        let mut seen_ports = std::collections::HashSet::new();
        for library in &mut config.libraries {
            if !library.library_path.is_absolute() {
                anyhow::bail!("library_path in config must be an absolute path");
            }
            if !seen_ports.insert(library.port) {
                anyhow::bail!("duplicate port {} across libraries in config", library.port);
            }
            std::fs::create_dir_all(&library.library_path)?;
            library.library_path = library.library_path.canonicalize()?;
        }

        Ok(config)
    }

    pub fn into_servers(self) -> Vec<ServerConfig> {
        self.libraries
            .into_iter()
            .map(|library| ServerConfig {
                host: self.host.clone(),
                port: library.port,
                api_token: self.api_token.clone(),
                library_path: library.library_path,
            })
            .collect()
    }
}
