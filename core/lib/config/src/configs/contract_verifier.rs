use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use smart_config::{metadata::TimeUnit, DescribeConfig, DeserializeConfig};

#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ContractVerifierConfig {
    /// Max time of a single compilation (in s).
    #[config(default_t = Duration::from_secs(240), with = TimeUnit::Seconds)]
    pub compilation_timeout: Duration,
    /// Port to which the Prometheus exporter server is listening.
    #[config(default_t = 3_318)]
    pub prometheus_port: u16,
    #[config(default_t = 3_070)]
    pub port: u16,
}

impl ContractVerifierConfig {
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), self.port)
    }
}

#[cfg(test)]
mod tests {
    use smart_config::{testing::test_complete, Environment, Yaml};

    use super::*;

    fn expected_config() -> ContractVerifierConfig {
        ContractVerifierConfig {
            compilation_timeout: Duration::from_secs(30),
            prometheus_port: 3314,
            port: 3070,
        }
    }

    #[test]
    fn parsing_from_env() {
        let env = r#"
            CONTRACT_VERIFIER_COMPILATION_TIMEOUT=30
            CONTRACT_VERIFIER_PROMETHEUS_PORT=3314
            CONTRACT_VERIFIER_PORT=3070
        "#;
        let env = Environment::from_dotenv("test.env", env)
            .unwrap()
            .strip_prefix("CONTRACT_VERIFIER_");

        let config: ContractVerifierConfig = test_complete(env).unwrap();
        assert_eq!(config, expected_config());
    }

    #[test]
    fn parsing_from_yaml() {
        let yaml = r#"
          port: 3070
          compilation_timeout: 30
          prometheus_port: 3314
        "#;
        let yaml = Yaml::new("test.yml", serde_yaml::from_str(yaml).unwrap()).unwrap();
        let config: ContractVerifierConfig = test_complete(yaml).unwrap();
        assert_eq!(config, expected_config());
    }
}
