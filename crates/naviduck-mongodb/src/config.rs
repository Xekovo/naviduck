//! Turns `ConnectParams` into `mongodb::options::ClientOptions` (142-mongodb.md §2).
//! Kept pure so unit tests can inspect assembled options without connecting.

use mongodb::Client;
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use naviduck_db::params::ConnectParams;
use naviduck_db::{DbError, DbResult};

pub(crate) fn client_options(params: &ConnectParams) -> DbResult<ClientOptions> {
    let host = if params.host.trim().is_empty() {
        "localhost".to_string()
    } else {
        params.host.trim().to_string()
    };
    let mut options = ClientOptions::default();
    options.hosts = vec![ServerAddress::Tcp {
        host,
        port: Some(params.port),
    }];
    options.app_name = Some("Naviduck".into());
    options.connect_timeout = Some(params.timeout());
    options.server_selection_timeout = Some(params.timeout());
    // When tunneled through SSH, the server's replica-set discovery hostnames are internal
    // to the remote network, so connect directly to the local tunnel endpoint.
    if params.tls_host.is_some() {
        options.direct_connection = Some(true);
    }
    if !params.user.is_empty() {
        let source = params
            .database
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("admin")
            .to_string();
        options.credential = Some(
            Credential::builder()
                .username(Some(params.user.clone()))
                .password(params.password.clone())
                .source(Some(source))
                .build(),
        );
    }
    if let Some(ssl) = &params.ssl {
        let tls = TlsOptions::builder()
            .ca_file_path(ssl.ca_cert.clone())
            .cert_key_file_path(ssl.client_key.clone().or_else(|| ssl.client_cert.clone()))
            .allow_invalid_certificates((!ssl.verify_server_cert).then_some(true))
            .build();
        options.tls = Some(Tls::Enabled(tls));
    }
    Ok(options)
}

pub(crate) fn build_client(params: &ConnectParams) -> DbResult<Client> {
    let options = client_options(params)?;
    Client::with_options(options).map_err(|e| DbError::msg(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use naviduck_model::connection::{Connection, SslMode};

    fn mongo_info() -> naviduck_db::DriverInfo {
        naviduck_db::DriverInfo {
            default_port: Some(27017),
            supports_network: true,
            supports_ssh: true,
            supports_ssl: true,
            initial_database: true,
            ..naviduck_db::DriverInfo::document("mongodb", "MongoDB")
        }
    }

    fn params(conn: &Connection) -> ConnectParams {
        ConnectParams::from_connection(conn, Default::default(), &mongo_info())
    }

    #[test]
    fn maps_host_port_and_defaults_auth_source_to_admin() {
        let mut conn = Connection::new("mongodb");
        conn.general.user = "alice".into();
        let mut p = params(&conn);
        p.password = Some("secret".into());
        let opts = client_options(&p).unwrap();
        assert_eq!(opts.hosts.len(), 1);
        assert_eq!(opts.hosts[0].to_string(), "localhost:27017");
        let cred = opts.credential.unwrap();
        assert_eq!(cred.username.as_deref(), Some("alice"));
        assert_eq!(cred.password.as_deref(), Some("secret"));
        assert_eq!(cred.source.as_deref(), Some("admin"));
        assert!(opts.tls.is_none());
        assert_eq!(opts.direct_connection, None);
    }

    #[test]
    fn initial_database_overrides_auth_source_and_ssh_sets_direct_connection() {
        let mut conn = Connection::new("mongodb");
        conn.general.user = "app".into();
        conn.general.database = Some("shop".into());
        let mut p = params(&conn);
        p.tls_host = Some("mongo.internal".into());
        let opts = client_options(&p).unwrap();
        assert_eq!(opts.credential.unwrap().source.as_deref(), Some("shop"));
        assert_eq!(opts.direct_connection, Some(true));
    }

    #[test]
    fn empty_user_omits_credential_and_ssl_maps_tls_options() {
        let mut conn = Connection::new("mongodb");
        conn.ssl.enabled = true;
        conn.ssl.mode = SslMode::Require;
        conn.ssl.use_auth = true;
        conn.ssl.ca_cert = Some("/tmp/ca.pem".into());
        conn.ssl.verify_server_cert = false;
        let opts = client_options(&params(&conn)).unwrap();
        assert!(opts.credential.is_none());
        let Some(Tls::Enabled(tls)) = opts.tls else {
            panic!("TLS should be enabled");
        };
        assert_eq!(
            tls.ca_file_path.as_deref(),
            Some(std::path::Path::new("/tmp/ca.pem"))
        );
        assert_eq!(tls.allow_invalid_certificates, Some(true));
    }
}
