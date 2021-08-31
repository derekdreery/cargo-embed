mod config;
mod error;
mod rttui;

use anyhow::{anyhow, Context, Result};
use chrono::Local;
use colored::*;
use std::{
    env, fs,
    fs::File,
    io::Write,
    iter, panic,
    path::Path,
    process,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use structopt::StructOpt;

use probe_rs::{
    config::TargetSelector,
    flashing::{download_file_with_options, DownloadOptions, FlashProgress, Format, ProgressEvent},
    DebugProbeSelector, Probe,
};
#[cfg(feature = "sentry")]
use probe_rs_cli_util::logging::{ask_to_log_crash, capture_anyhow, capture_panic};

use probe_rs_cli_util::{
    build_artifact,
    common_options::CargoOptions,
    indicatif::{MultiProgress, ProgressBar, ProgressStyle},
    logging::{self, Metadata},
};

use probe_rs_rtt::{Rtt, ScanRegion};

use crate::rttui::channel::DataFormat;

lazy_static::lazy_static! {
    static ref METADATA: Arc<Mutex<Metadata>> = Arc::new(Mutex::new(Metadata {
        release: CARGO_VERSION.to_string(),
        chip: None,
        probe: None,
        speed: None,
        commit: GIT_VERSION.to_string()
    }));
}

const CARGO_NAME: &str = env!("CARGO_PKG_NAME");
const CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_VERSION: &str = git_version::git_version!(fallback = "crates.io");

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(short = "V", long = "version")]
    pub version: bool,
    #[structopt(name = "config")]
    config: Option<String>,
    #[structopt(name = "chip", long = "chip")]
    chip: Option<String>,
    #[structopt(
        long = "probe",
        help = "Use this flag to select a specific probe in the list.\n\
        Use '--probe VID:PID' or '--probe VID:PID:Serial' if you have more than one probe with the same VID:PID."
    )]
    probe_selector: Option<DebugProbeSelector>,
    #[structopt(name = "list-chips", long = "list-chips")]
    list_chips: bool,
    #[structopt(name = "disable-progressbars", long = "disable-progressbars")]
    disable_progressbars: bool,
    /// Always flash the program, even when it hasn't changed.
    #[structopt(long)]
    flash_when_fresh: bool,

    // `cargo build` arguments
    #[structopt(flatten)]
    cargo_options: CargoOptions,
}

fn main() {
    let next = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        #[cfg(feature = "sentry")]
        if ask_to_log_crash() {
            capture_panic(&METADATA.lock().unwrap(), &info)
        }
        #[cfg(not(feature = "sentry"))]
        log::info!("{:#?}", &METADATA.lock().unwrap());
        next(info);
    }));

    match main_try() {
        Ok(_) => (),
        Err(e) => {
            // Ensure stderr is flushed before calling proces::exit,
            // otherwise the process might panic, because it tries
            // to access stderr during shutdown.
            //
            // We ignore the errors, not much we can do anyway.

            let mut stderr = std::io::stderr();

            let first_line_prefix = "Error".red().bold();
            let other_line_prefix: String = iter::repeat(" ")
                .take(first_line_prefix.chars().count())
                .collect();

            let error = format!("{:?}", e);

            for (i, line) in error.lines().enumerate() {
                let _ = write!(stderr, "       ");

                if i == 0 {
                    let _ = write!(stderr, "{}", first_line_prefix);
                } else {
                    let _ = write!(stderr, "{}", other_line_prefix);
                };

                let _ = writeln!(stderr, " {}", line);
            }

            let _ = stderr.flush();

            #[cfg(feature = "sentry")]
            if ask_to_log_crash() {
                capture_anyhow(&METADATA.lock().unwrap(), &e)
            }
            #[cfg(not(feature = "sentry"))]
            log::info!("{:#?}", &METADATA.lock().unwrap());

            process::exit(1);
        }
    }
}

fn main_try() -> Result<()> {
    let mut args = std::env::args();

    // When called by Cargo, the first argument after the binary name will be `embed`. If that's the
    // case, remove one argument (`Opt::from_iter` will remove the binary name by itself).
    if env::args().nth(1) == Some("embed".to_string()) {
        args.next();
    }

    // Get commandline options.
    let opt = Opt::from_iter(args);

    if opt.version {
        println!(
            "{} {}\ngit commit: {}",
            CARGO_NAME, CARGO_VERSION, GIT_VERSION
        );
        return Ok(());
    }

    let work_dir = std::env::current_dir()?;

    // Get the config.
    let config_name = opt.config.as_deref().unwrap_or("default");
    let configs = config::Configs::new(work_dir.clone());
    let config = configs.select_defined(config_name)?;

    logging::init(Some(config.general.log_level));

    // Make sure we load the config given in the cli parameters.
    for cdp in &config.general.chip_descriptions {
        probe_rs::config::add_target_from_yaml(&Path::new(cdp))
            .with_context(|| format!("failed to load the chip description from {}", cdp))?;
    }

    let chip = if opt.list_chips {
        print_families()?;
        std::process::exit(0);
    } else {
        opt.chip
            .or_else(|| config.general.chip.clone())
            .map(|chip| chip.into())
            .unwrap_or(TargetSelector::Auto)
    };

    METADATA.lock().unwrap().chip = Some(format!("{:?}", chip));

    let artifact = build_artifact(&work_dir, &opt.cargo_options.to_cargo_options())?;

    // Get the binary name (without extension) from the build artifact path
    let name = artifact
        .path()
        .file_stem()
        .and_then(|f| f.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Unable to determine binary file name from path {}",
                artifact.path().display()
            )
        })?;

    logging::println(format!("      {} {}", "Config".green().bold(), config_name));
    logging::println(format!(
        "      {} {}",
        "Target".green().bold(),
        artifact.path().display()
    ));

    // If we got a probe selector in the config, open the probe matching the selector if possible.
    let mut probe = if let Some(selector) = opt.probe_selector {
        Probe::open(selector)?
    } else {
        match (config.probe.usb_vid.as_ref(), config.probe.usb_pid.as_ref()) {
            (Some(vid), Some(pid)) => {
                let selector = DebugProbeSelector {
                    vendor_id: u16::from_str_radix(vid, 16)?,
                    product_id: u16::from_str_radix(pid, 16)?,
                    serial_number: config.probe.serial.clone(),
                };
                // if two probes with the same VID:PID pair exist we just choose one
                Probe::open(selector)?
            }
            _ => {
                if config.probe.usb_vid.is_some() {
                    log::warn!("USB VID ignored, because PID is not specified.");
                }
                if config.probe.usb_pid.is_some() {
                    log::warn!("USB PID ignored, because VID is not specified.");
                }

                // Only automatically select a probe if there is only
                // a single probe detected.
                let list = Probe::list_all();
                if list.len() > 1 {
                    return Err(anyhow!("The following devices were found:\n \
                                    {} \
                                        \
                                    Use '--probe VID:PID'\n \
                                                            \
                                    You can also set the [default.probe] config attribute \
                                    (in your Embed.toml) to select which probe to use. \
                                    For usage examples see https://github.com/probe-rs/cargo-embed/blob/master/src/config/default.toml .",
                                    list.iter().enumerate().map(|(num, link)| format!("[{}]: {:?}\n", num, link)).collect::<String>()));
                }
                Probe::open(
                    list.first()
                        .map(|info| {
                            METADATA.lock().unwrap().probe = Some(format!("{:?}", info.probe_type));
                            info
                        })
                        .ok_or_else(|| anyhow!("No supported probe was found"))?,
                )?
            }
        }
    };

    probe
        .select_protocol(config.probe.protocol)
        .context("failed to select protocol")?;

    let protocol_speed = if let Some(speed) = config.probe.speed {
        let actual_speed = probe.set_speed(speed).context("failed to set speed")?;

        if actual_speed < speed {
            log::warn!(
                "Unable to use specified speed of {} kHz, actual speed used is {} kHz",
                speed,
                actual_speed
            );
        }

        actual_speed
    } else {
        probe.speed_khz()
    };

    METADATA.lock().unwrap().speed = Some(format!("{:?}", protocol_speed));

    log::info!("Protocol speed {} kHz", protocol_speed);

    let mut session = if config.general.connect_under_reset {
        probe
            .attach_under_reset(chip)
            .context("failed attaching to target")?
    } else {
        let potential_session = probe.attach(chip);
        match potential_session {
            Ok(session) => session,
            Err(err) => {
                log::info!("The target seems to be unable to be attached to.");
                log::info!(
                    "A hard reset during attaching might help. This will reset the entire chip."
                );
                log::info!("Set `general.connect_under_reset` in your cargo-embed configuration file to enable this feature.");
                return Err(err).context("failed attaching to target");
            }
        }
    };

    if config.flashing.enabled && (!artifact.fresh() || opt.flash_when_fresh) {
        // Start timer.
        let instant = Instant::now();

        if !opt.disable_progressbars {
            // Create progress bars.
            let multi_progress = MultiProgress::new();
            let style = ProgressStyle::default_bar()
                .tick_chars("⠁⠁⠉⠙⠚⠒⠂⠂⠒⠲⠴⠤⠄⠄⠤⠠⠠⠤⠦⠖⠒⠐⠐⠒⠓⠋⠉⠈⠈✔")
                .progress_chars("##-")
                .template("{msg:.green.bold} {spinner} [{elapsed_precise}] [{wide_bar}] {bytes:>8}/{total_bytes:>8} @ {bytes_per_sec:>10} (eta {eta:3})");

            // Create a new progress bar for the fill progress if filling is enabled.
            let fill_progress = if config.flashing.restore_unwritten_bytes {
                let fill_progress = Arc::new(multi_progress.add(ProgressBar::new(0)));
                fill_progress.set_style(style.clone());
                fill_progress.set_message("     Reading flash  ");
                Some(fill_progress)
            } else {
                None
            };

            // Create a new progress bar for the erase progress.
            let erase_progress = Arc::new(multi_progress.add(ProgressBar::new(0)));
            {
                logging::set_progress_bar(erase_progress.clone());
            }
            erase_progress.set_style(style.clone());
            erase_progress.set_message("     Erasing sectors");

            // Create a new progress bar for the program progress.
            let program_progress = multi_progress.add(ProgressBar::new(0));
            program_progress.set_style(style);
            program_progress.set_message(" Programming pages  ");

            let flash_layout_output_path = config.flashing.flash_layout_output_path.clone();
            // Register callback to update the progress.
            let progress = FlashProgress::new(move |event| {
                use ProgressEvent::*;
                match event {
                    Initialized { flash_layout } => {
                        let total_page_size: u32 =
                            flash_layout.pages().iter().map(|s| s.size()).sum();
                        let total_sector_size: u32 =
                            flash_layout.sectors().iter().map(|s| s.size()).sum();
                        let total_fill_size: u32 =
                            flash_layout.fills().iter().map(|s| s.size()).sum();
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.set_length(total_fill_size as u64)
                        }
                        erase_progress.set_length(total_sector_size as u64);
                        program_progress.set_length(total_page_size as u64);
                        let visualizer = flash_layout.visualize();
                        flash_layout_output_path
                            .as_ref()
                            .map(|path| visualizer.write_svg(path));
                    }
                    StartedProgramming => {
                        program_progress.enable_steady_tick(100);
                        program_progress.reset_elapsed();
                    }
                    StartedErasing => {
                        erase_progress.enable_steady_tick(100);
                        erase_progress.reset_elapsed();
                    }
                    StartedFilling => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.enable_steady_tick(100)
                        };
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.reset_elapsed()
                        };
                    }
                    PageProgrammed { size, .. } => {
                        program_progress.inc(size as u64);
                    }
                    SectorErased { size, .. } => {
                        erase_progress.inc(size as u64);
                    }
                    PageFilled { size, .. } => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.inc(size as u64)
                        };
                    }
                    FailedErasing => {
                        erase_progress.abandon();
                        program_progress.abandon();
                    }
                    FinishedErasing => {
                        erase_progress.finish();
                    }
                    FailedProgramming => {
                        program_progress.abandon();
                    }
                    FinishedProgramming => {
                        program_progress.finish();
                    }
                    FailedFilling => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.abandon()
                        };
                    }
                    FinishedFilling => {
                        if let Some(fp) = fill_progress.as_ref() {
                            fp.finish()
                        };
                    }
                }
            });

            // Make the multi progresses print.
            // indicatif requires this in a separate thread as this join is a blocking op,
            // but is required for printing multiprogress.
            let progress_thread_handle = std::thread::spawn(move || {
                multi_progress.join().unwrap();
            });

            let mut options = DownloadOptions::new();

            options.progress = Some(&progress);
            options.keep_unwritten_bytes = config.flashing.restore_unwritten_bytes;
            options.do_chip_erase = config.flashing.do_chip_erase;

            download_file_with_options(&mut session, artifact.path(), Format::Elf, options)
                .with_context(|| format!("failed to flash {}", artifact.path().display()))?;

            // We don't care if we cannot join this thread.
            let _ = progress_thread_handle.join();

            // If we don't do this, the inactive progress bars will swallow log
            // messages, so they'll never be printed anywhere.
            logging::clear_progress_bar();
        } else {
            let mut options = DownloadOptions::new();
            options.keep_unwritten_bytes = config.flashing.restore_unwritten_bytes;
            options.do_chip_erase = config.flashing.do_chip_erase;

            download_file_with_options(&mut session, artifact.path(), Format::Elf, options)
                .with_context(|| format!("failed to flash {}", artifact.path().display()))?;
        }

        // Stop timer.
        let elapsed = instant.elapsed();
        logging::println(format!(
            "    {} flashing in {}s",
            "Finished".green().bold(),
            elapsed.as_millis() as f32 / 1000.0,
        ));
    }

    if config.reset.enabled {
        let mut core = session.core(0)?;
        let halt_timeout = Duration::from_millis(500);
        #[allow(deprecated)] // Remove in 0.10
        if config.flashing.halt_afterwards {
            logging::eprintln(format!(
                "     {} The 'flashing.halt_afterwards' option in the config has moved to the 'reset' section",
                "Warning".yellow().bold()
            ));
            core.reset_and_halt(halt_timeout)?;
        } else if config.reset.halt_afterwards {
            core.reset_and_halt(halt_timeout)?;
        } else {
            core.reset()?;
        }
    }

    let session = Arc::new(Mutex::new(session));

    let mut gdb_thread_handle = None;
    if config.gdb.enabled {
        let gdb_connection_string = config.gdb.gdb_connection_string.clone();
        let session = session.clone();
        gdb_thread_handle = Some(std::thread::spawn(move || {
            let gdb_connection_string = gdb_connection_string.as_deref().or(Some("127.0.0.1:1337"));
            // This next unwrap will always resolve as the connection string is always Some(T).
            log::info!(
                "Firing up GDB stub at {}.",
                gdb_connection_string.as_ref().unwrap(),
            );
            if let Err(e) = probe_rs_gdb_server::run(gdb_connection_string, &session) {
                logging::eprintln("During the execution of GDB an error was encountered:");
                logging::eprintln(format!("{:?}", e));
            }
        }));
    }
    if config.rtt.enabled {
        let defmt_enable = config
            .rtt
            .channels
            .iter()
            .any(|elem| elem.format == DataFormat::Defmt);
        let defmt_state = if defmt_enable {
            let elf = fs::read(artifact.path()).unwrap();
            let table = defmt_decoder::Table::parse(&elf)?;

            let locs = {
                let table = table.as_ref().unwrap();
                let locs = table.get_locations(&elf)?;

                if !table.is_empty() && locs.is_empty() {
                    log::warn!("Insufficient DWARF info; compile your program with `debug = 2` to enable location info.");
                    None
                } else if table.indices().all(|idx| locs.contains_key(&(idx as u64))) {
                    Some(locs)
                } else {
                    log::warn!("Location info is incomplete; it will be omitted from the output.");
                    None
                }
            };
            Some((table.unwrap(), locs))
        } else {
            None
        };

        let t = std::time::Instant::now();
        let mut error = None;

        let mut i = 1;

        while (t.elapsed().as_millis() as usize) < config.rtt.timeout {
            log::info!("Initializing RTT (attempt {})...", i);
            i += 1;

            let rtt_header_address = if let Ok(mut file) = File::open(artifact.path()) {
                if let Some(address) = rttui::app::App::get_rtt_symbol(&mut file) {
                    ScanRegion::Exact(address as u32)
                } else {
                    ScanRegion::Ram
                }
            } else {
                ScanRegion::Ram
            };

            match Rtt::attach_region(session.clone(), &rtt_header_address) {
                Ok(rtt) => {
                    log::info!("RTT initialized.");

                    // `App` puts the terminal into a special state, as required
                    // by the text-based UI. If a panic happens while the
                    // terminal is in that state, this will completely mess up
                    // the user's terminal (misformatted panic message, newlines
                    // being ignored, input characters not being echoed, ...).
                    //
                    // The following panic hook cleans up the terminal, while
                    // otherwise preserving the behavior of the default panic
                    // hook (or whichever custom hook might have been registered
                    // before).
                    let previous_panic_hook = panic::take_hook();
                    panic::set_hook(Box::new(move |panic_info| {
                        rttui::app::clean_up_terminal();
                        previous_panic_hook(panic_info);
                    }));

                    let chip_name = config.general.chip.as_deref().unwrap_or_default();
                    let logname = format!("{}_{}_{}", name, chip_name, Local::now().to_rfc3339());
                    let mut app = rttui::app::App::new(rtt, &config, logname)?;
                    loop {
                        app.poll_rtt();
                        app.render(&defmt_state);
                        if app.handle_event() {
                            logging::println("Shutting down.");
                            return Ok(());
                        };
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(err) => {
                    error = Some(anyhow!("Error attaching to RTT: {}", err));
                }
            };

            log::debug!("Failed to initialize RTT. Retrying until timeout.");
        }
        if let Some(error) = error {
            return Err(error);
        }
    }

    if let Some(gdb_thread_handle) = gdb_thread_handle {
        let _ = gdb_thread_handle.join();
    }

    logging::println(format!(
        "        {} processing config {}",
        "Done".green().bold(),
        config_name
    ));

    Ok(())
}

fn print_families() -> Result<()> {
    logging::println("Available chips:");
    for family in
        probe_rs::config::families().map_err(|e| anyhow!("Families could not be read: {:?}", e))?
    {
        logging::println(&family.name);
        logging::println("    Variants:");
        for variant in family.variants() {
            logging::println(format!("        {}", variant.name));
        }
    }
    Ok(())
}
