//! [TUI (Terminal User Interface)](https://en.wikipedia.org/wiki/Text-based_user_interface) subsystem for Caligula.

pub mod cli;
mod fancy_ui;
mod simple_ui;
mod utils;
mod wizard;

use std::{fs::File, sync::Arc};

use tracing::{debug, info};

pub use self::{cli::BurnArgs, utils::ByteSpeed};
use crate::{
    codec::compression::CompressionFormat,
    facade::{CaligulaFacade, WVState, WriteVerifyWorkflow},
    logging::LogPaths,
    tui::{
        simple_ui::do_setup_wizard,
        utils::{TUICapture, TermiosRestore},
    },
    util::runtime::RemoteSpawn,
};

/// Entrypoint for both TUI-based UIs.
pub fn main(
    runtime: impl RemoteSpawn,
    facade: Arc<impl CaligulaFacade>,
    log_paths: Arc<LogPaths>,
    args: BurnArgs,
) -> anyhow::Result<()> {
    let _termios_restore = match File::open("/dev/tty") {
        Ok(tty) => TermiosRestore::new(tty).ok(),
        Err(error) => {
            info!(
                ?error,
                "failed to open /dev/tty, will not attempt to restore after program"
            );
            None
        }
    };

    // no image given, pick everything in the full-screen wizard
    if args.image.is_none()
        && let Some(catalog) = args.catalog.clone()
    {
        return wizard_main(&runtime, facade, &log_paths, &catalog);
    }

    let Some(start_write_verify) = do_setup_wizard(&runtime, facade.clone(), &args)? else {
        return Ok(());
    };

    let child_state = simple_ui::try_start_write_or_escalate(
        facade.clone(),
        &runtime,
        &start_write_verify,
        args.root,
        args.interactive.is_interactive(),
    )?;

    if args.interactive.is_interactive() {
        let mut tui = TUICapture::new()?;
        let terminal = tui.terminal();
        // create app and run it
        fancy_ui::run(
            runtime,
            fancy_ui::Params {
                terminal,
                begin: &start_write_verify,
                child_state,
                terminal_events: crossterm::event::EventStream::new(),
                log_paths: &log_paths,
            },
        );
    } else {
        simple_ui::run(simple_ui::Params {
            child_state,
            log_paths: &log_paths,
        });
    }

    debug!("Done!");
    Ok(())
}

/// Wizard flow: wizard, then one write + verify per disk (several for a
/// floppy set, with an "insert the next floppy" screen in between).
fn wizard_main(
    runtime: &impl RemoteSpawn,
    facade: Arc<impl CaligulaFacade>,
    log_paths: &LogPaths,
    catalog_url: &str,
) -> anyhow::Result<()> {
    eprintln!("Loading the image list from {catalog_url} ...");
    let catalog = wizard::Catalog::fetch(catalog_url)?;

    let mut tui = TUICapture::new()?;
    let Some(plan) = wizard::run(tui.terminal(), &catalog)? else {
        return Ok(());
    };

    for (n, disk) in plan.disks.iter().enumerate() {
        if n > 0 && !wizard::insert_disk(tui.terminal(), &plan, n)? {
            break;
        }
        if !wizard::ready_floppy(tui.terminal(), &plan.target)? {
            break;
        }
        let input = std::path::PathBuf::from(&disk.url);
        let compression =
            CompressionFormat::detect_from_path(&input).unwrap_or(CompressionFormat::Identity);
        let begin = WriteVerifyWorkflow::new(input, compression, plan.target.clone())?;
        let child_state = simple_ui::try_start_write_or_escalate(
            facade.clone(),
            runtime,
            &begin,
            cli::UseSudo::Never,
            true,
        )?;
        let watch = child_state.clone();
        fancy_ui::run(
            runtime,
            fancy_ui::Params {
                terminal: tui.terminal(),
                begin: &begin,
                child_state,
                terminal_events: crossterm::event::EventStream::new(),
                log_paths,
            },
        );
        let failed = match &*watch.borrow() {
            WVState::Finished { result: Err(e), .. } => Some(e.to_string()),
            WVState::Finished { .. } => None,
            _ => Some("the write was interrupted".into()),
        };
        if let Some(error) = failed {
            wizard::notice(
                tui.terminal(),
                "The write failed",
                vec![
                    format!("{}: {error}", disk.name),
                    String::new(),
                    format!("Details are in {}", log_paths.main()),
                ],
            )?;
            break;
        }
    }
    Ok(())
}
