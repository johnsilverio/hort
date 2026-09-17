//! The databases a sandbox declares, decided over the configuration alone.
//!
//! Inside a sandbox every declared database is one loopback port, so what the
//! configuration can get wrong about them is knowable before anything is built.

use super::config::ResolvedConfig;
use super::error::HortError;
use crate::ports::DbForward;

/// The databases the configuration declares, refusing a port declared for more
/// than one of them.
pub fn declared_databases(config: &ResolvedConfig) -> Result<Vec<DbForward>, HortError> {
    let declarations: Vec<DbForward> = config
        .network
        .iter()
        .map(|database| DbForward { host: database.host.clone(), port: database.port })
        .collect();
    one_database_per_port(&declarations)
}

/// These databases with each destination once, refusing a port declared for
/// two different hosts.
pub fn one_database_per_port(forwards: &[DbForward]) -> Result<Vec<DbForward>, HortError> {
    let mut declared: Vec<DbForward> = Vec::new();
    for forward in forwards {
        match declared.iter().find(|other| other.port == forward.port) {
            Some(other) if other.host != forward.host => {
                return Err(HortError::DatabasesShareAPort {
                    port: forward.port,
                    first: other.host.clone(),
                    second: forward.host.clone(),
                });
            }
            Some(_) => {}
            None => declared.push(DbForward { host: forward.host.clone(), port: forward.port }),
        }
    }
    Ok(declared)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::config::{Cache, Mounts, Network};

    fn database(host: &str, port: u16) -> Network {
        Network { mode: "network".to_string(), host: host.to_string(), port }
    }

    fn config_declaring(databases: Vec<Network>) -> ResolvedConfig {
        ResolvedConfig {
            rootfs: Some("/base/rootfs".to_string()),
            agents: Vec::new(),
            mounts: Mounts::default(),
            network: databases,
            egress: None,
            notifications: None,
            cache: Cache::default(),
            shell: None,
            resources: None,
        }
    }

    #[test]
    fn two_databases_declared_on_one_port_are_refused_naming_both() {
        let config =
            config_declaring(vec![database("10.255.255.1", 5432), database("10.255.255.2", 5432)]);

        let refusal = declared_databases(&config).err();

        // Inside the sandbox both are the same address, so one of the two
        // declarations is a promise hort cannot keep, and nothing in the
        // configuration says which.
        assert_eq!(
            refusal.map(|error| error.to_string()),
            Some(
                "two databases are declared on port 5432 (10.255.255.1 and 10.255.255.2), and a sandbox can reach only one of them — remove one from \"network\" in your configuration or give it another port"
                    .to_string()
            )
        );
    }

    #[test]
    fn one_database_declared_twice_on_its_port_is_accepted() {
        let config =
            config_declaring(vec![database("10.255.255.1", 5432), database("10.255.255.1", 5432)]);

        let declared = declared_databases(&config);

        // Two declarations of one destination name one address and one
        // database, so there is nothing to choose between. A single file can
        // carry the pair, since only the merge of two layers dedupes it.
        assert!(declared.is_ok());
    }

    #[test]
    fn databases_declared_on_different_ports_are_all_declared() {
        let config =
            config_declaring(vec![database("10.255.255.1", 5432), database("10.255.255.2", 3306)]);

        let declared = declared_databases(&config).unwrap();

        assert_eq!(
            declared
                .iter()
                .map(|forward| (forward.host.as_str(), forward.port))
                .collect::<Vec<_>>(),
            vec![("10.255.255.1", 5432), ("10.255.255.2", 3306)]
        );
    }
}
