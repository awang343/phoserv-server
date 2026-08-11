use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub library_path: PathBuf,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub api_token: String,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    4173
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
                 library_path = \"/path/to/test-library\"\n\
                 host = \"127.0.0.1\"\n\
                 port = 4173\n\
                 api_token = \"<some-secret-token>\"\n",
                path.display()
            )
        })?;
        let mut config: Config = toml::from_str(&contents)?;
        if !config.library_path.is_absolute() {
            anyhow::bail!("library_path in config must be an absolute path");
        }
        std::fs::create_dir_all(&config.library_path)?;
        config.library_path = config.library_path.canonicalize()?;
        Ok(config)
    }
}
