fn run() -> Result<(), failure::Error> {
    let mut config = relay_config::Config::default();
    // TODO: deduplicate with relay::cli::extract_config_env_vars
    config.apply_override(relay_config::OverridableConfig {
        id: std::env::var("RELAY_ID").ok(),
        public_key: std::env::var("RELAY_PUBLIC_KEY").ok(),
        secret_key: std::env::var("RELAY_SECRET_KEY").ok(),
        upstream: std::env::var("RELAY_UPSTREAM_URL").ok(),
        host: Some("127.0.0.1".to_string()),
        port: Some("0".to_string()),
        processing: Some("false".to_string()),
        ..Default::default()
    })?;
    assert_eq!(config.relay_mode(), relay_config::RelayMode::Managed);
    if !config.has_credentials() {
        // TODO: warn and switch to proxy mode instead of failing?
        return Err(failure::err_msg(
            "Relay sidecar misconfigured: missing credentials",
        ));
    }
    relay_log::init(config.logging(), config.sentry());
    relay_server::run(config)?;
    Ok(())
}

fn main() {
    let exit_code = match run() {
        Ok(()) => 0,
        Err(err) => {
            relay_log::ensure_error(&err);
            1
        }
    };

    relay_log::Hub::current().client().map(|x| x.close(None));
    std::process::exit(exit_code);
}
