use crate::utils::context::Context;
use crate::utils::locale::system_locale;
use crate::utils::log::{progress, CliLogger};
use anyhow::bail;
use anyhow::Result;
use clap::{Parser, Subcommand};
use crunchyroll_rs::crunchyroll::CrunchyrollBuilder;
use crunchyroll_rs::error::Error;
use crunchyroll_rs::{Crunchyroll, Locale};
use log::{debug, error, warn, LevelFilter};
use reqwest::{Client, Proxy};
use std::{env, fs};

mod archive;
mod download;
mod login;
mod search;
mod utils;

use crate::utils::rate_limit::RateLimiterService;
pub use archive::Archive;
use dialoguer::console::Term;
pub use download::Download;
pub use login::Login;
pub use search::Search;

trait Execute {
    fn pre_check(&mut self) -> Result<()> {
        Ok(())
    }
    async fn execute(self, ctx: Context) -> Result<()>;
}

#[derive(Debug, Parser)]
#[clap(author, version = version(), about)]
#[clap(name = "crunchy-cli")]
pub struct Cli {
    #[clap(flatten)]
    verbosity: Verbosity,

    #[arg(
        help = "Overwrite the language in which results are returned. Default is your system language"
    )]
    #[arg(global = true, long)]
    lang: Option<Locale>,

    #[arg(
        help = "Enable experimental fixes which may resolve some unexpected errors. Generally not recommended as this flag may crash the program completely"
    )]
    #[arg(
        long_help = "Enable experimental fixes which may resolve some unexpected errors. \
            It is not recommended to use this this flag regularly, it might cause unexpected errors which may crash the program completely. \
            If everything works as intended this option isn't needed, but sometimes Crunchyroll mislabels \
            the audio of a series/season or episode or returns a wrong season number. This is when using this option might help to solve the issue"
    )]
    #[arg(global = true, long, default_value_t = false)]
    experimental_fixes: bool,

    #[clap(flatten)]
    login_method: login::LoginMethod,

    #[arg(help = "Use a proxy to route all traffic through")]
    #[arg(long_help = "Use a proxy to route all traffic through. \
            Make sure that the proxy can either forward TLS requests, which is needed to bypass the (cloudflare) bot protection, or that it is configured so that the proxy can bypass the protection itself. \
            Besides specifying a simple url, you also can partially control where a proxy should be used: '<url>:' only proxies api requests, ':<url>' only proxies download traffic, '<url>:<url>' proxies api requests through the first url and download traffic through the second url")]
    #[arg(global = true, long, value_parser = crate::utils::clap::clap_parse_proxies)]
    proxy: Option<(Option<Proxy>, Option<Proxy>)>,

    #[arg(help = "Use custom user agent")]
    #[arg(global = true, long)]
    user_agent: Option<String>,

    #[arg(
        help = "Maximal speed to download/request (may be a bit off here and there). Must be in format of <number>[B|KB|MB]"
    )]
    #[arg(
        long_help = "Maximal speed to download/request (may be a bit off here and there). Must be in format of <number>[B|KB|MB] (e.g. 500KB or 10MB)"
    )]
    #[arg(global = true, long, value_parser = crate::utils::clap::clap_parse_speed_limit)]
    speed_limit: Option<u32>,

    #[clap(subcommand)]
    command: Command,
}

fn version() -> String {
    let package_version = env!("CARGO_PKG_VERSION");
    let git_commit_hash = env!("GIT_HASH");
    let build_date = env!("BUILD_DATE");

    if git_commit_hash.is_empty() {
        package_version.to_string()
    } else {
        format!("{} ({} {})", package_version, git_commit_hash, build_date)
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    Archive(Archive),
    Download(Download),
    Login(Login),
    Search(Search),
}

#[derive(Debug, Parser)]
struct Verbosity {
    #[arg(help = "Verbose output")]
    #[arg(global = true, short, long)]
    verbose: bool,

    #[arg(help = "Quiet output. Does not print anything unless it's a error")]
    #[arg(
        long_help = "Quiet output. Does not print anything unless it's a error. Can be helpful if you pipe the output to stdout"
    )]
    #[arg(global = true, short, long)]
    quiet: bool,
}

pub async fn main(args: &[String]) {
    let mut cli: Cli = Cli::parse_from(args);

    if cli.verbosity.verbose || cli.verbosity.quiet {
        if cli.verbosity.verbose && cli.verbosity.quiet {
            eprintln!("Output cannot be verbose ('-v') and quiet ('-q') at the same time");
            std::process::exit(1)
        } else if cli.verbosity.verbose {
            CliLogger::init(LevelFilter::Debug).unwrap()
        } else if cli.verbosity.quiet {
            CliLogger::init(LevelFilter::Error).unwrap()
        }
    } else {
        CliLogger::init(LevelFilter::Info).unwrap()
    }

    debug!("cli input: {:?}", cli);

    match &mut cli.command {
        Command::Archive(archive) => {
            // prevent interactive select to be shown when output should be quiet
            if cli.verbosity.quiet {
                archive.yes = true;
            }
            pre_check_executor(archive).await
        }
        Command::Download(download) => {
            // prevent interactive select to be shown when output should be quiet
            if cli.verbosity.quiet {
                download.yes = true;
            }
            pre_check_executor(download).await
        }
        Command::Login(login) => {
            if login.remove {
                if let Some(session_file) = login::session_file_path() {
                    let _ = fs::remove_file(session_file);
                }
                return;
            } else {
                pre_check_executor(login).await
            }
        }
        Command::Search(search) => pre_check_executor(search).await,
    };

    let ctx = match create_ctx(&mut cli).await {
        Ok(ctx) => ctx,
        Err(e) => {
            error!("{}", e);
            std::process::exit(1)
        }
    };
    debug!("Created context");

    ctrlc::set_handler(move || {
        debug!("Ctrl-c detected");
        if let Ok(dir) = fs::read_dir(env::temp_dir()) {
            for file in dir.flatten() {
                if file
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default()
                    .starts_with(".crunchy-cli_")
                {
                    if file.file_type().map_or(true, |ft| ft.is_file()) {
                        let result = fs::remove_file(file.path());
                        debug!(
                            "Ctrl-c removed temporary file {} {}",
                            file.path().to_string_lossy(),
                            if result.is_ok() {
                                "successfully"
                            } else {
                                "not successfully"
                            }
                        )
                    } else {
                        let result = fs::remove_dir_all(file.path());
                        debug!(
                            "Ctrl-c removed temporary directory {} {}",
                            file.path().to_string_lossy(),
                            if result.is_ok() {
                                "successfully"
                            } else {
                                "not successfully"
                            }
                        )
                    }
                }
            }
        }
        // when pressing ctrl-c while interactively choosing seasons the cursor stays hidden, this
        // line shows it again
        let _ = Term::stdout().show_cursor();
        std::process::exit(1)
    })
    .unwrap();
    debug!("Created ctrl-c handler");

    match cli.command {
        Command::Archive(archive) => execute_executor(archive, ctx).await,
        Command::Download(download) => execute_executor(download, ctx).await,
        Command::Login(login) => execute_executor(login, ctx).await,
        Command::Search(search) => execute_executor(search, ctx).await,
    };
}

async fn pre_check_executor(executor: &mut impl Execute) {
    if let Err(err) = executor.pre_check() {
        error!("Misconfigurations detected: {}", err);
        std::process::exit(1)
    }
}

async fn execute_executor(executor: impl Execute, ctx: Context) {
    if let Err(mut err) = executor.execute(ctx).await {
        if let Some(crunchy_error) = err.downcast_mut::<Error>() {
            if let Error::Block { message, .. } = crunchy_error {
                *message = "Triggered Cloudflare bot protection. Try again later or use a VPN or proxy to spoof your location".to_string()
            }

            error!("An error occurred: {}", crunchy_error)
        } else {
            error!("An error occurred: {}", err)
        }

        std::process::exit(1)
    }
}

async fn create_ctx(cli: &mut Cli) -> Result<Context> {
    let crunchy_client = reqwest_client(
        cli.proxy.as_ref().and_then(|p| p.0.clone()),
        cli.user_agent.clone(),
    );
    let internal_client = reqwest_client(
        cli.proxy.as_ref().and_then(|p| p.1.clone()),
        cli.user_agent.clone(),
    );

    let crunchy = crunchyroll_session(
        cli,
        crunchy_client.clone(),
        cli.speed_limit
            .map(|l| RateLimiterService::new(l, crunchy_client)),
    )
    .await?;

    Ok(Context {
        crunchy,
        client: internal_client.clone(),
        rate_limiter: cli
            .speed_limit
            .map(|l| RateLimiterService::new(l, internal_client)),
    })
}

async fn crunchyroll_session(
    cli: &mut Cli,
    client: Client,
    rate_limiter: Option<RateLimiterService>,
) -> Result<Crunchyroll> {
    let supported_langs = vec![
        Locale::zh_CN,
        Locale::zh_HK,
        Locale::zh_TW,
        Locale::en_US,
        Locale::ar_ME,
        Locale::de_DE,
        Locale::es_ES,
        Locale::es_419,
        Locale::fr_FR,
        Locale::it_IT,
        Locale::pt_BR,
        Locale::pt_PT,
        Locale::ru_RU,
    ];
    let locale = if let Some(lang) = &cli.lang {
        if !supported_langs.contains(lang) {
            bail!(
                "Via `--lang` specified language is not supported. Supported languages: {}",
                supported_langs
                    .iter()
                    .map(|l| format!("`{}` ({})", l, l.to_human_readable()))
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        }
        lang.clone()
    } else {
        let mut lang = system_locale();
        if !supported_langs.contains(&lang) {
            warn!("Recognized system locale is not supported. Using en-US as default. Use `--lang` to overwrite the used language");
            lang = Locale::en_US
        }
        lang
    };

    let mut builder = Crunchyroll::builder()
        .locale(locale)
        .client(client.clone())
        .stabilization_locales(cli.experimental_fixes)
        .stabilization_season_number(cli.experimental_fixes);
    if let Command::Download(download) = &cli.command {
        builder = builder.preferred_audio_locale(download.audio.clone())
    }
    if let Some(rate_limiter) = rate_limiter {
        builder = builder.middleware(rate_limiter)
    }

    let root_login_methods_count =
        cli.login_method.credentials.is_some() as u8 + cli.login_method.anonymous as u8;

    let progress_handler = progress!("Logging in");
    if root_login_methods_count == 0 {
        if let Some(login_file_path) = login::session_file_path() {
            if login_file_path.exists() {
                let session = fs::read_to_string(login_file_path)?;
                if let Some((token_type, token)) = session.split_once(':') {
                    match token_type {
                        "refresh_token" => {
                            return match builder.login_with_refresh_token(token).await {
                                Ok(crunchy) => Ok(crunchy),
                                Err(e) => {
                                    if let Error::Request { message, .. } = &e {
                                        if message.starts_with("invalid_grant") {
                                            bail!("The stored login is expired, please login again")
                                        }
                                    }
                                    Err(e.into())
                                }
                            }
                        }
                        "etp_rt" => bail!("The stored login method (etp-rt) isn't supported anymore. Please login again using your credentials"),
                        _ => (),
                    }
                }
                bail!("Could not read stored session ('{}')", session)
            }
        }
        bail!("Please use a login method ('--credentials' or '--anonymous')")
    } else if root_login_methods_count > 1 {
        bail!("Please use only one login method ('--credentials' or '--anonymous')")
    }

    let crunchy = if let Some(credentials) = &cli.login_method.credentials {
        if let Some((email, password)) = credentials.split_once(':') {
            builder.login_with_credentials(email, password).await?
        } else {
            bail!("Invalid credentials format. Please provide your credentials as email:password")
        }
    } else if cli.login_method.anonymous {
        builder.login_anonymously().await?
    } else {
        bail!("should never happen")
    };

    progress_handler.stop("Logged in");

    Ok(crunchy)
}

fn reqwest_client(proxy: Option<Proxy>, user_agent: Option<String>) -> Client {
    let mut builder = CrunchyrollBuilder::predefined_client_builder();
    if let Some(p) = proxy {
        builder = builder.proxy(p)
    }
    if let Some(ua) = user_agent {
        builder = builder.user_agent(ua)
    }

    #[cfg(any(feature = "openssl-tls", feature = "openssl-tls-static"))]
    let client = {
        let mut builder = builder.use_native_tls().tls_built_in_root_certs(false);

        for certificate in rustls_native_certs::load_native_certs().unwrap() {
            builder =
                builder.add_root_certificate(reqwest::Certificate::from_der(&certificate).unwrap())
        }

        builder.build().unwrap()
    };
    #[cfg(not(any(feature = "openssl-tls", feature = "openssl-tls-static")))]
    let client = builder.build().unwrap();

    client
}
