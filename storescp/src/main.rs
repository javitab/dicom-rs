use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
};

use clap::Parser;
use dicom_core::{dicom_value, DataElement, VR};
use dicom_dictionary_std::tags;
use dicom_object::{InMemDicomObject, StandardDataDictionary};
use snafu::Report;
use tracing::{error, info, Level};

mod store_async;
mod store_sync;
mod transfer;
use store_async::run_store_async;
use store_sync::run_store_sync;

/// DICOM C-STORE SCP
#[derive(Debug, Parser)]
#[command(version)]
struct App {
    /// Verbose mode
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
    /// Calling Application Entity title
    #[arg(long = "calling-ae-title", default_value = "STORE-SCP")]
    calling_ae_title: String,
    /// Enforce max pdu length
    #[arg(short = 's', long = "strict")]
    strict: bool,
    /// Only accept native/uncompressed transfer syntaxes
    #[arg(long)]
    uncompressed_only: bool,
    /// Accept unknown SOP classes
    #[arg(long)]
    promiscuous: bool,
    /// Maximum PDU length
    #[arg(
        short = 'm',
        long = "max-pdu-length",
        default_value = "16384",
        value_parser(clap::value_parser!(u32).range(4096..=131_072))
    )]
    max_pdu_length: u32,
    /// Output directory for incoming objects
    #[arg(short = 'o', default_value = ".")]
    out_dir: PathBuf,
    /// Which port to listen on
    #[arg(short, default_value = "11111")]
    port: u16,
    /// Run in non-blocking mode (spins up an async task to handle each incoming stream)
    #[arg(short, long)]
    non_blocking: bool,
    /// HTTP URL to send DICOM status updates to (requires --status-auth or --status-no-auth)
    #[arg(long, default_value = "")]
    status_url: String,
    /// Authorization header value for HTTP status updates (required and non-empty if status-url is provided, e.g., "Bearer token123")
    #[arg(long)]
    status_auth: Option<String>,
    /// Disable authentication requirement for HTTP status updates (allows empty or no auth header)
    #[arg(long)]
    status_no_auth: bool,
    /// Disable SSL certificate verification for HTTP status updates (use with caution)
    #[arg(long)]
    status_insecure: bool,
    /// Print detailed DICOM object information for each stored instance
    #[arg(long)]
    show_details: bool,
}

fn create_cstore_response(
    message_id: u16,
    sop_class_uid: &str,
    sop_instance_uid: &str,
) -> InMemDicomObject<StandardDataDictionary> {
    InMemDicomObject::command_from_element_iter([
        DataElement::new(
            tags::AFFECTED_SOP_CLASS_UID,
            VR::UI,
            dicom_value!(Str, sop_class_uid),
        ),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8001])),
        DataElement::new(
            tags::MESSAGE_ID_BEING_RESPONDED_TO,
            VR::US,
            dicom_value!(U16, [message_id]),
        ),
        DataElement::new(
            tags::COMMAND_DATA_SET_TYPE,
            VR::US,
            dicom_value!(U16, [0x0101]),
        ),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(
            tags::AFFECTED_SOP_INSTANCE_UID,
            VR::UI,
            dicom_value!(Str, sop_instance_uid),
        ),
    ])
}

fn create_cecho_response(message_id: u16) -> InMemDicomObject<StandardDataDictionary> {
    InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8030])),
        DataElement::new(
            tags::MESSAGE_ID_BEING_RESPONDED_TO,
            VR::US,
            dicom_value!(U16, [message_id]),
        ),
        DataElement::new(
            tags::COMMAND_DATA_SET_TYPE,
            VR::US,
            dicom_value!(U16, [0x0101]),
        ),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
    ])
}

fn main() {
    let app = App::parse();
    
    // Validate that status_auth is provided and not empty if status_url is not empty (unless status_no_auth is set)
    if !app.status_url.is_empty() && !app.status_no_auth {
        if app.status_auth.is_none() {
            error!("--status-auth is required when --status-url is provided (or use --status-no-auth to disable authentication)");
            std::process::exit(-1);
        }
        if let Some(ref auth) = app.status_auth {
            if auth.is_empty() {
                error!("--status-auth cannot be empty when --status-url is provided (or use --status-no-auth to disable authentication)");
                std::process::exit(-1);
            }
        }
    }
    
    if app.non_blocking {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                run_async(app).await.unwrap_or_else(|e| {
                    error!("{:?}", e);
                    std::process::exit(-2);
                });
            });
    } else {
        run_sync(app).unwrap_or_else(|e| {
            error!("{:?}", e);
            std::process::exit(-2);
        });
    }
}

async fn run_async(args: App) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    let args = Arc::new(args);
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(if args.verbose {
                Level::DEBUG
            } else {
                Level::INFO
            })
            .finish(),
    )
    .unwrap_or_else(|e| {
        eprintln!(
            "Could not set up global logger: {}",
            snafu::Report::from_error(e)
        );
    });

    std::fs::create_dir_all(&args.out_dir).unwrap_or_else(|e| {
        error!("Could not create output directory: {}", e);
        std::process::exit(-2);
    });

    let listen_addr = SocketAddrV4::new(Ipv4Addr::from(0), args.port);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    info!(
        "{} listening on: tcp://{}",
        &args.calling_ae_title, listen_addr
    );

    loop {
        let (socket, _addr) = listener.accept().await?;
        let args = args.clone();
        tokio::task::spawn(async move {
            if let Err(e) = run_store_async(socket, &args).await {
                error!("{}", Report::from_error(e));
            }
        });
    }
}

fn run_sync(args: App) -> Result<(), Box<dyn std::error::Error>> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(if args.verbose {
                Level::DEBUG
            } else {
                Level::INFO
            })
            .finish(),
    )
    .unwrap_or_else(|e| {
        eprintln!(
            "Could not set up global logger: {}",
            snafu::Report::from_error(e)
        );
    });

    std::fs::create_dir_all(&args.out_dir).unwrap_or_else(|e| {
        error!("Could not create output directory: {}", e);
        std::process::exit(-2);
    });

    let listen_addr = SocketAddrV4::new(Ipv4Addr::from(0), args.port);
    let listener = std::net::TcpListener::bind(listen_addr)?;
    info!(
        "{} listening on: tcp://{}",
        &args.calling_ae_title, listen_addr
    );

    for stream in listener.incoming() {
        match stream {
            Ok(scu_stream) => {
                if let Err(e) = run_store_sync(scu_stream, &args) {
                    error!("{}", snafu::Report::from_error(e));
                }
            }
            Err(e) => {
                error!("{}", snafu::Report::from_error(e));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::App;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        App::command().debug_assert();
    }
}
