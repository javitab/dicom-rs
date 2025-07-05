use dicom_dictionary_std::tags;
use dicom_dictionary_std::StandardSopClassDictionary;
use dicom_core::dictionary::{UidDictionary, UidDictionaryEntry};
use dicom_core::header::Header;
use dicom_encoding::transfer_syntax::TransferSyntaxIndex;
use dicom_object::{FileMetaTableBuilder, InMemDicomObject};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::{pdu::PDataValueType, Pdu};
use snafu::{OptionExt, Report, ResultExt, Whatever};
use tracing::{debug, info, warn};
use serde::Serialize;
use chrono::{DateTime, Utc};

use crate::{create_cecho_response, create_cstore_response, transfer::ABSTRACT_SYNTAXES, App};

fn translate_sop_class_uid(sop_class_uid: &str) -> String {
    let dict = StandardSopClassDictionary;
    if let Some(entry) = dict.by_uid(sop_class_uid) {
        entry.name().to_string()
    } else {
        // Fall back to the UID if not found in dictionary
        sop_class_uid.to_string()
    }
}

#[derive(Serialize)]
struct DicomStatus {
    timestamp: DateTime<Utc>,
    sop_instance_uid: String,
    sop_class_uid: String,
    sop_class: String,
    study_instance_uid: String,
    series_instance_uid: String,
    patient_id: Option<String>,
    study_accession_number: Option<String>,
    status: String,
    message: Option<String>,
    file_path: Option<String>,
}
pub async fn run_store_async(
    scu_stream: tokio::net::TcpStream,
    args: &App,
) -> Result<(), Whatever> {
    let App {
        verbose,
        calling_ae_title,
        strict,
        uncompressed_only,
        promiscuous,
        max_pdu_length,
        out_dir,
        port: _,
        non_blocking: _,
        status_url,
        status_auth,
        status_no_auth,
        status_insecure,
        show_details,
    } = args;
    let verbose = *verbose;

    let mut instance_buffer: Vec<u8> = Vec::with_capacity(1024 * 1024);
    let mut msgid = 1;
    let mut sop_class_uid = String::new();
    let mut sop_instance_uid = String::new();

    let mut options = dicom_ul::association::ServerAssociationOptions::new()
        .accept_any()
        .ae_title(calling_ae_title)
        .strict(*strict)
        .max_pdu_length(*max_pdu_length)
        .promiscuous(*promiscuous);

    if *uncompressed_only {
        options = options
            .with_transfer_syntax("1.2.840.10008.1.2")
            .with_transfer_syntax("1.2.840.10008.1.2.1");
    } else {
        for ts in TransferSyntaxRegistry.iter() {
            if !ts.is_unsupported() {
                options = options.with_transfer_syntax(ts.uid());
            }
        }
    };

    for uid in ABSTRACT_SYNTAXES {
        options = options.with_abstract_syntax(*uid);
    }

    let mut association = options
        .establish_async(scu_stream)
        .await
        .whatever_context("could not establish association")?;

    info!("New association from {}", association.client_ae_title());
    debug!(
        "> Presentation contexts: {:?}",
        association.presentation_contexts()
    );

    loop {
        match association.receive().await {
            Ok(mut pdu) => {
                if verbose {
                    debug!("scu ----> scp: {}", pdu.short_description());
                }
                match pdu {
                    Pdu::PData { ref mut data } => {
                        if data.is_empty() {
                            debug!("Ignoring empty PData PDU");
                            continue;
                        }

                        for data_value in data {
                            if data_value.value_type == PDataValueType::Data && !data_value.is_last
                            {
                                instance_buffer.append(&mut data_value.data);
                            } else if data_value.value_type == PDataValueType::Command
                                && data_value.is_last
                            {
                                // commands are always in implicit VR LE
                                let ts =
                                    dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN
                                        .erased();
                                let data_value = &data_value;
                                let v = &data_value.data;

                                let obj = InMemDicomObject::read_dataset_with_ts(v.as_slice(), &ts)
                                    .whatever_context("failed to read incoming DICOM command")?;
                                let command_field = obj
                                    .element(tags::COMMAND_FIELD)
                                    .whatever_context("Missing Command Field")?
                                    .uint16()
                                    .whatever_context("Command Field is not an integer")?;

                                if command_field == 0x0030 {
                                    // Handle C-ECHO-RQ
                                    let cecho_response = create_cecho_response(msgid);
                                    let mut cecho_data = Vec::new();

                                    cecho_response
                                        .write_dataset_with_ts(&mut cecho_data, &ts)
                                        .whatever_context(
                                            "could not write C-ECHO response object",
                                        )?;

                                    let pdu_response = Pdu::PData {
                                        data: vec![dicom_ul::pdu::PDataValue {
                                            presentation_context_id: data_value
                                                .presentation_context_id,
                                            value_type: PDataValueType::Command,
                                            is_last: true,
                                            data: cecho_data,
                                        }],
                                    };
                                    association.send(&pdu_response).await.whatever_context(
                                        "failed to send C-ECHO response object to SCU",
                                    )?;
                                } else {
                                    msgid = obj
                                        .element(tags::MESSAGE_ID)
                                        .whatever_context("Missing Message ID")?
                                        .to_int()
                                        .whatever_context("Message ID is not an integer")?;
                                    sop_class_uid = obj
                                        .element(tags::AFFECTED_SOP_CLASS_UID)
                                        .whatever_context("missing Affected SOP Class UID")?
                                        .to_str()
                                        .whatever_context(
                                            "could not retrieve Affected SOP Class UID",
                                        )?
                                        .to_string();
                                    sop_instance_uid = obj
                                        .element(tags::AFFECTED_SOP_INSTANCE_UID)
                                        .whatever_context("missing Affected SOP Instance UID")?
                                        .to_str()
                                        .whatever_context(
                                            "could not retrieve Affected SOP Instance UID",
                                        )?
                                        .to_string();
                                }
                                instance_buffer.clear();                            } else if data_value.value_type == PDataValueType::Data
                                && data_value.is_last
                            {
                                instance_buffer.append(&mut data_value.data);

                                let presentation_context = association
                                    .presentation_contexts()
                                    .iter()
                                    .find(|pc| pc.id == data_value.presentation_context_id)
                                    .whatever_context("missing presentation context")?;
                                let ts = &presentation_context.transfer_syntax;

                                // Process the DICOM object and handle errors
                                let (file_path_opt, study_uid_opt, series_uid_opt, patient_id_opt, accession_opt, error_msg_opt) = match process_dicom_instance(
                                    &instance_buffer,
                                    ts,
                                    &sop_class_uid,
                                    &sop_instance_uid,
                                    out_dir,
                                    *show_details,
                                ).await {
                                    Ok((file_path, study_uid, series_uid, patient_id, accession_number)) => {
                                        info!("Stored {}", file_path.display());
                                        (Some(file_path), Some(study_uid), Some(series_uid), patient_id, accession_number, None)
                                    }
                                    Err(_) => {
                                        let error_msg = "Failed to process DICOM instance".to_string();
                                        warn!("{}", error_msg);
                                        (None, None, None, None, None, Some(error_msg))
                                    }
                                };
                                
                                // Send HTTP status update
                                let status = if let Some(file_path) = file_path_opt {
                                    DicomStatus {
                                        timestamp: Utc::now(),
                                        sop_instance_uid: sop_instance_uid.trim_end_matches('\0').to_string(),
                                        sop_class_uid: sop_class_uid.clone(),
                                        sop_class: translate_sop_class_uid(&sop_class_uid),
                                        study_instance_uid: study_uid_opt.unwrap().trim_end_matches('\0').to_string(),
                                        series_instance_uid: series_uid_opt.unwrap().trim_end_matches('\0').to_string(),
                                        patient_id: patient_id_opt,
                                        study_accession_number: accession_opt,
                                        status: "success".to_string(),
                                        message: Some("DICOM instance stored successfully".to_string()),
                                        file_path: Some(file_path.to_string_lossy().to_string()),
                                    }
                                } else {
                                    DicomStatus {
                                        timestamp: Utc::now(),
                                        sop_instance_uid: sop_instance_uid.trim_end_matches('\0').to_string(),
                                        sop_class_uid: sop_class_uid.clone(),
                                        sop_class: translate_sop_class_uid(&sop_class_uid),
                                        study_instance_uid: "unknown".to_string(),
                                        series_instance_uid: "unknown".to_string(),
                                        patient_id: None,
                                        study_accession_number: None,
                                        status: "failed".to_string(),
                                        message: error_msg_opt,
                                        file_path: None,
                                    }
                                };
                                send_dicom_status(status, status_url, status_auth.as_deref(), *status_no_auth, *status_insecure, verbose).await;

                                // send C-STORE-RSP object
                                // commands are always in implicit VR LE
                                let ts =
                                    dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN
                                        .erased();

                                let obj = create_cstore_response(
                                    msgid,
                                    &sop_class_uid,
                                    &sop_instance_uid,
                                );

                                let mut obj_data = Vec::new();

                                obj.write_dataset_with_ts(&mut obj_data, &ts)
                                    .whatever_context("could not write response object")?;

                                let pdu_response = Pdu::PData {
                                    data: vec![dicom_ul::pdu::PDataValue {
                                        presentation_context_id: data_value.presentation_context_id,
                                        value_type: PDataValueType::Command,
                                        is_last: true,
                                        data: obj_data,
                                    }],
                                };
                                association
                                    .send(&pdu_response)
                                    .await
                                    .whatever_context("failed to send response object to SCU")?;
                            }
                        }
                    }
                    Pdu::ReleaseRQ => {
                        association.send(&Pdu::ReleaseRP).await.unwrap_or_else(|e| {
                            warn!(
                                "Failed to send association release message to SCU: {}",
                                snafu::Report::from_error(e)
                            );
                        });
                        info!(
                            "Released association with {}",
                            association.client_ae_title()
                        );
                        break;
                    }
                    Pdu::AbortRQ { source } => {
                        warn!("Aborted connection from: {:?}", source);
                        break;
                    }
                    _ => {}
                }
            }
            Err(err @ dicom_ul::association::server::Error::Receive { .. }) => {
                if verbose {
                    info!("{}", Report::from_error(err));
                } else {
                    info!("{}", err);
                }
                break;
            }
            Err(err) => {
                warn!("Unexpected error: {}", Report::from_error(err));
                break;
            }
        }
    }

    if let Ok(peer_addr) = association.inner_stream().peer_addr() {
        info!(
            "Dropping connection with {} ({})",
            association.client_ae_title(),
            peer_addr
        );
    } else {
        info!("Dropping connection with {}", association.client_ae_title());
    }

    Ok(())
}

async fn send_dicom_status(status: DicomStatus, url: &str, auth_header: Option<&str>, no_auth: bool, insecure: bool, verbose: bool) {
    // Always display JSON in verbose mode, regardless of whether we send HTTP request
    if verbose {
        match serde_json::to_string_pretty(&status) {
            Ok(json_content) => {
                info!("DICOM status JSON:\n{}", json_content);
            }
            Err(e) => {
                warn!("Failed to serialize status for logging: {}", e);
            }
        }
    }

    // Only send HTTP request if URL is provided and not empty
    if url.is_empty() {
        if verbose {
            info!("No status URL provided, skipping HTTP request");
        }
        return;
    }

    if verbose {
        info!("Sending HTTP POST to: {}", url);
        if !no_auth && auth_header.is_some() {
            info!("Using authorization header");
        } else if no_auth {
            info!("Authentication disabled for this request");
        }
        if insecure {
            info!("SSL certificate verification disabled");
        }
    }

    // Create HTTP client with optional SSL verification
    let client = if insecure {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_else(|e| {
                warn!("Failed to create insecure HTTP client, falling back to secure: {}", e);
                reqwest::Client::new()
            })
    } else {
        reqwest::Client::new()
    };
    
    let mut request = client.post(url).json(&status);
    
    // Add authorization header if provided and not disabled
    if !no_auth {
        if let Some(auth) = auth_header {
            request = request.header("Authorization", auth);
        }
    }

    match request.send().await {
        Ok(response) => {
            if response.status().is_success() {
                if verbose {
                    info!("HTTP POST successful - Status: {}, SOP Instance UID: {}", 
                          response.status(), status.sop_instance_uid);
                } else {
                    debug!("Successfully sent status update for SOP Instance UID: {}", status.sop_instance_uid);
                }
            } else {
                warn!("Failed to send status update, HTTP status: {}", response.status());
                if verbose {
                    if let Ok(body) = response.text().await {
                        warn!("HTTP response body: {}", body);
                    }
                }
            }
        }
        Err(e) => {
            warn!("Failed to send status update: {}", e);
            if verbose {
                warn!("HTTP request details - URL: {}, Error: {}", url, e);
            }
        }
    }
}

async fn process_dicom_instance(
    instance_buffer: &[u8],
    ts: &str,
    _sop_class_uid: &str,
    sop_instance_uid: &str,
    out_dir: &std::path::PathBuf,
    show_details: bool,
) -> Result<(std::path::PathBuf, String, String, Option<String>, Option<String>), Whatever> {
    let obj = InMemDicomObject::read_dataset_with_ts(
        instance_buffer,
        TransferSyntaxRegistry.get(ts).unwrap(),
    )
    .whatever_context("failed to read DICOM data object")?;

    // Extract study and series instance UIDs from the DICOM object
    let study_instance_uid = obj
        .element(tags::STUDY_INSTANCE_UID)
        .whatever_context("missing Study Instance UID")?
        .to_str()
        .whatever_context("could not retrieve Study Instance UID")?
        .to_string();
    
    let series_instance_uid = obj
        .element(tags::SERIES_INSTANCE_UID)
        .whatever_context("missing Series Instance UID")?
        .to_str()
        .whatever_context("could not retrieve Series Instance UID")?
        .to_string();

    // Extract patient ID and study accession number from the original object
    let patient_id = obj
        .element(tags::PATIENT_ID)
        .ok()
        .and_then(|elem| elem.to_str().ok())
        .map(|s| s.trim_end_matches('\0').to_string())
        .filter(|s| !s.is_empty());
    
    let study_accession_number = obj
        .element(tags::ACCESSION_NUMBER)
        .ok()
        .and_then(|elem| elem.to_str().ok())
        .map(|s| s.trim_end_matches('\0').to_string())
        .filter(|s| !s.is_empty());

    // Print detailed DICOM object information if requested
    if show_details {
        print_dicom_details(&obj, sop_instance_uid);
    }

    let file_meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(
            obj.element(tags::SOP_CLASS_UID)
                .whatever_context("missing SOP Class UID")?
                .to_str()
                .whatever_context("could not retrieve SOP Class UID")?,
        )
        .media_storage_sop_instance_uid(
            obj.element(tags::SOP_INSTANCE_UID)
                .whatever_context("missing SOP Instance UID")?
                .to_str()
                .whatever_context("missing SOP Instance UID")?,
        )
        .transfer_syntax(ts)
        .build()
        .whatever_context("failed to build DICOM meta file information")?;
    
    let file_obj = obj.with_exact_meta(file_meta);

    // Create the directory structure and file path
    let mut file_path = out_dir.clone();
    file_path.push(format!(
        "{}/{}/{}.dcm",
        study_instance_uid.trim_end_matches('\0'),
        series_instance_uid.trim_end_matches('\0'),
        sop_instance_uid.trim_end_matches('\0')
    ));
    
    // Ensure the directory exists
    if let Some(parent_dir) = file_path.parent() {
        std::fs::create_dir_all(parent_dir)
            .whatever_context("failed to create directory structure")?;
    }
    
    file_obj
        .write_to_file(&file_path)
        .whatever_context("could not save DICOM object to file")?;

    Ok((file_path, study_instance_uid, series_instance_uid, patient_id, study_accession_number))
}

fn print_dicom_details(obj: &InMemDicomObject, sop_instance_uid: &str) {
    // Print basic information
    info!("SOP Instance UID: {}", sop_instance_uid);
    if let Ok(elem) = obj.element(tags::PATIENT_ID) {
        if let Ok(patient_id) = elem.to_str() {
            info!("Patient ID: {}", patient_id);
        }
    }
    if let Ok(elem) = obj.element(tags::STUDY_INSTANCE_UID) {
        if let Ok(study_uid) = elem.to_str() {
            info!("Study Instance UID: {}", study_uid);
        }
    }
    if let Ok(elem) = obj.element(tags::SERIES_INSTANCE_UID) {
        if let Ok(series_uid) = elem.to_str() {
            info!("Series Instance UID: {}", series_uid);
        }
    }
    if let Ok(elem) = obj.element(tags::ACCESSION_NUMBER) {
        if let Ok(accession_number) = elem.to_str() {
            info!("Study Accession Number: {}", accession_number);
        }
    }

    // Print all elements in the dataset
    info!("DICOM Object Details:");
    for elem in obj.iter() {
        let tag = elem.tag();
        let vr = elem.vr();
        let value = elem.value();
        
        info!("{} ({}): {:?}", tag, vr, value);
    }
}
