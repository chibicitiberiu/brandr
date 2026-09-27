//! netflash's full-screen setup wizard (used with `--catalog`):
//!
//!   1 Image   pick an image from the network catalog
//!   2 Target  pick a whole disk (or partition one and come back)
//!   3 Method  how to write it; the image kind picks the default
//!   4 Review  what will happen, confirm
//!
//! Esc goes back one step. The write itself then runs in the existing progress
//! UI; multi-disk floppy sets loop with an "insert disk N" screen in between.

mod catalog;
mod devices;
mod ui;

use std::io;

use ratatui::{Terminal, prelude::Backend, text::Line};

pub use self::catalog::{Catalog, CatalogImage};
use self::ui::{Chrome, Col, ListResult, ListSpec, Row, dialog, field, field_more, list_screen};
use crate::util::device::WriteTarget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// byte-for-byte copy of one image
    Raw,
    /// a floppy image inside a USB-HDD layout (syslinux + memdisk), built by the server
    UsbHdd,
    /// the same in ZIP-drive layout (partition 4, 64 heads / 32 sectors)
    UsbZip,
    /// a CD-only ISO as FAT32 + files that UEFI boots (Windows: split WIM, BIOS too)
    Uefi,
    /// every disk of a set in turn, onto a floppy drive
    FloppySet,
    /// DOS-bootable FAT + files (Rufus "MS-DOS" mode), not built yet
    DosBootable,
}

impl Method {
    const ALL: [Method; 6] = [
        Method::Raw,
        Method::Uefi,
        Method::UsbHdd,
        Method::UsbZip,
        Method::FloppySet,
        Method::DosBootable,
    ];

    fn name(self) -> &'static str {
        match self {
            Method::Raw => "Raw copy",
            Method::UsbHdd => "Floppy as USB-HDD",
            Method::UsbZip => "Floppy as USB-ZIP",
            Method::Uefi => "UEFI stick (FAT32 + files)",
            Method::FloppySet => "Floppy set",
            Method::DosBootable => "DOS-bootable disk + files",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Method::Raw => "copy the image byte for byte, then verify",
            Method::UsbHdd => "stick boots the floppy via syslinux + memdisk",
            Method::UsbZip => "same, in ZIP-drive layout for 2000-era BIOSes",
            Method::Uefi => "the ISO's files on FAT32, bootable on UEFI",
            Method::FloppySet => "write every disk of the set, swapping floppies",
            Method::DosBootable => "FAT disk that boots DOS, with the CD's files",
        }
    }

    /// Server-side variant name for methods that write a built layout.
    fn variant(self) -> Option<&'static str> {
        match self {
            Method::UsbHdd => Some("usb-hdd"),
            Method::UsbZip => Some("usb-zip"),
            Method::Uefi => Some("uefi"),
            _ => None,
        }
    }

    /// Why this method can't be used here, if it can't.
    fn unavailable(self, image: &CatalogImage, target: &WriteTarget) -> Option<&'static str> {
        let floppy_drive = devices::is_floppy(target);
        match self {
            Method::Raw => None,
            Method::UsbHdd | Method::UsbZip if image.kind != "floppy" => {
                Some("only for floppy images")
            }
            Method::UsbHdd | Method::UsbZip if floppy_drive => Some("needs a USB stick or disk"),
            Method::UsbHdd | Method::UsbZip if image.variant(self.variant().unwrap()).is_none() => {
                Some("server can't build it")
            }
            Method::UsbHdd | Method::UsbZip => None,
            Method::Uefi if image.variant("uefi").is_none() => {
                Some("only for CD-only ISOs with EFI boot")
            }
            Method::Uefi if floppy_drive => Some("needs a USB stick or disk"),
            Method::Uefi
                if image
                    .variant("uefi")
                    .is_some_and(|v| v.state.as_deref() == Some("error")) =>
            {
                Some("server couldn't build it (see its /status)")
            }
            Method::Uefi => None,
            Method::FloppySet if !image.is_set() => Some("not part of a disk set"),
            Method::FloppySet if !floppy_drive => Some("needs a floppy drive"),
            Method::FloppySet => None,
            Method::DosBootable => Some("coming next"),
        }
    }

    fn default_for(image: &CatalogImage, target: &WriteTarget) -> Method {
        [Method::FloppySet, Method::UsbHdd, Method::Uefi]
            .into_iter()
            .find(|m| m.unavailable(image, target).is_none())
            .unwrap_or(Method::Raw)
    }

    /// How the written result boots.
    fn boot_hint(self, image: &CatalogImage, target: &WriteTarget) -> String {
        match self {
            Method::UsbHdd | Method::UsbZip | Method::Uefi => image
                .variant(self.variant().unwrap())
                .map_or_else(String::new, |v| v.boot_hint.clone()),
            _ if image.kind == "floppy" && devices::is_floppy(target) => "floppy".into(),
            _ if image.kind == "floppy" => "USB-FDD (the stick acts as a floppy)".into(),
            _ => image.boot_hint.clone(),
        }
    }

    /// The images to write for this method.
    fn disks(self, catalog: &Catalog, image: &CatalogImage) -> Vec<CatalogImage> {
        match self {
            Method::FloppySet => catalog.set_from(image),
            m if m.variant().is_some() => {
                let v = image
                    .variant(m.variant().unwrap())
                    .expect("checked by unavailable()");
                let mut built = image.clone();
                built.url = v.url.clone();
                built.size = v.size.unwrap_or(0); // uefi: filled in once the server built it
                built.boot_hint = v.boot_hint.clone();
                vec![built]
            }
            _ => vec![image.clone()],
        }
    }
}

/// Everything the wizard decided.
pub struct Plan {
    pub target: WriteTarget,
    pub method: Method,
    /// images to write, in order (one, or the rest of a floppy set)
    pub disks: Vec<CatalogImage>,
}

enum Step {
    Image,
    Target,
    Method,
    Review,
}

/// Run the wizard. `None` if the user backed out of the first screen.
pub fn run<B: Backend>(term: &mut Terminal<B>, catalog: &Catalog) -> anyhow::Result<Option<Plan>> {
    let mut step = Step::Image;
    let mut image: Option<usize> = None;
    let mut target: Option<WriteTarget> = None;
    let mut method: Option<Method> = None;

    loop {
        step = match step {
            Step::Image => match pick_image(term, catalog, image)? {
                Some(i) => {
                    if image != Some(i) {
                        method = None; // new image, new default
                    }
                    image = Some(i);
                    Step::Target
                }
                None => return Ok(None),
            },
            Step::Target => {
                let img = &catalog.images[image.expect("picked")];
                match pick_target(term, img, target.as_ref())? {
                    Some(t) => {
                        if target.as_ref() != Some(&t) {
                            method = None;
                        }
                        target = Some(t);
                        Step::Method
                    }
                    None => Step::Image,
                }
            }
            Step::Method => {
                let img = &catalog.images[image.expect("picked")];
                let t = target.as_ref().expect("picked");
                match pick_method(term, img, t, method)? {
                    Some(m) => {
                        method = Some(m);
                        Step::Review
                    }
                    None => Step::Target,
                }
            }
            Step::Review => {
                let img = &catalog.images[image.expect("picked")];
                let t = target.clone().expect("picked");
                let m = method.expect("picked");
                let mut disks = m.disks(catalog, img);
                if m == Method::Uefi {
                    match prepare_variant(term, img, &t)? {
                        Some((size, hint)) => {
                            disks[0].size = size;
                            disks[0].boot_hint = hint;
                        }
                        None => {
                            step = Step::Method;
                            continue;
                        }
                    }
                }
                let plan = Plan {
                    target: t,
                    method: m,
                    disks,
                };
                if review(term, img, &plan)? {
                    return Ok(Some(plan));
                }
                Step::Method
            }
        };
    }
}

fn pick_image<B: Backend>(
    term: &mut Terminal<B>,
    catalog: &Catalog,
    previous: Option<usize>,
) -> io::Result<Option<usize>> {
    // One row per image; disks 2..N of a set are reached through the set's first disk
    let mut rows = Vec::new();
    let mut index = Vec::new(); // row -> catalog index
    let mut section = String::new();
    for (i, img) in catalog.images.iter().enumerate() {
        if img.set_index.is_some_and(|n| n > 1) {
            continue;
        }
        if img.section != section {
            section = img.section.clone();
            rows.push(Row::Section(section.clone()));
            index.push(usize::MAX);
        }
        let name = match img.set_size {
            Some(n) if n > 1 => format!("{}  (set of {n})", img.name),
            _ => img.name.clone(),
        };
        let mut detail = vec![
            field("Image", img.name.clone()),
            field_more(img.section.clone()),
            field(
                "Kind",
                format!("{}, {}", img.kind_label(), img.size_label()),
            ),
        ];
        detail.push(if img.kind == "cd-only" {
            Line::styled(
                "            CD-only: written raw it won't boot from a disk",
                ui::warn_style(),
            )
        } else {
            field("Boots as", img.boot_hint.clone())
        });
        rows.push(Row::Item {
            cols: vec![name, img.size_label(), img.kind_label().to_string()],
            disabled: false,
            detail,
            search: format!("{} {} {}", img.name, img.section, img.kind_label()),
        });
        index.push(i);
    }
    let initial = previous.and_then(|p| index.iter().position(|&i| i == p));
    let spec = ListSpec {
        chrome: Chrome {
            step: Chrome::step(1, "Image"),
            heading: "Pick an image to write",
            heading_right: String::new(),
            keys: "Up/Down move   Enter select   type to filter   Esc quit",
        },
        layout: vec![Col::Flex, Col::Right(10), Col::Left(12)],
        rows,
        filterable: true,
        initial,
        detail_height: 4,
    };
    Ok(match list_screen(term, spec)? {
        ListResult::Selected(r) => Some(index[r]),
        ListResult::Back => None,
    })
}

enum TargetRow {
    Disk(WriteTarget),
    Partition,
    Refresh,
}

fn pick_target<B: Backend>(
    term: &mut Terminal<B>,
    image: &CatalogImage,
    previous: Option<&WriteTarget>,
) -> anyhow::Result<Option<WriteTarget>> {
    let mut remembered = previous.cloned();
    loop {
        let disks = devices::targets();
        let mut rows = vec![Row::Section("Disks".into())];
        let mut actions = vec![None];
        for d in &disks {
            let fits = devices::capacity(d).is_none_or(|c| c >= image.size);
            let removable = if d.removable == crate::util::device::Removable::Yes {
                ", removable"
            } else {
                ""
            };
            let mut detail = vec![
                field(
                    "Device",
                    format!(
                        "{}  ({}{removable})",
                        d.devnode.to_string_lossy(),
                        devices::bus(d)
                    ),
                ),
                field("Model", d.model.to_string()),
                field("Size", devices::size_label(d)),
            ];
            if !fits {
                detail.push(Line::styled(
                    format!(
                        "            Too small: the image needs {}",
                        image.size_label()
                    ),
                    ui::warn_style(),
                ));
            }
            rows.push(Row::Item {
                cols: vec![
                    d.name.clone(),
                    if devices::is_floppy(d) {
                        String::new()
                    } else {
                        devices::size_label(d)
                    },
                    devices::bus(d).to_string(),
                    if devices::is_floppy(d) {
                        "floppy drive".into()
                    } else {
                        d.model.to_string()
                    },
                ],
                disabled: !fits,
                detail,
                search: format!("{} {} {}", d.name, d.model, devices::bus(d)),
            });
            actions.push(Some(TargetRow::Disk(d.clone())));
        }
        rows.push(Row::Section("Actions".into()));
        actions.push(None);
        for (label, what, action) in [
            (
                "Partition a disk...",
                "Opens cfdisk on a disk you pick, then comes back here.",
                TargetRow::Partition,
            ),
            (
                "Refresh",
                "Look for disks again (e.g. after plugging in a USB stick).",
                TargetRow::Refresh,
            ),
        ] {
            rows.push(Row::Item {
                cols: vec![label.into()],
                disabled: false,
                detail: vec![Line::raw(what)],
                search: label.into(),
            });
            actions.push(Some(action));
        }
        let initial = remembered.as_ref().and_then(|r| {
            actions
                .iter()
                .position(|a| matches!(a, Some(TargetRow::Disk(d)) if d.devnode == r.devnode))
        });
        let spec = ListSpec {
            chrome: Chrome {
                step: Chrome::step(2, "Target"),
                heading: "Pick the device to write to",
                heading_right: String::new(),
                keys: "Up/Down move   Enter select   Esc back",
            },
            layout: vec![Col::Left(8), Col::Right(10), Col::Left(7), Col::Flex],
            rows,
            filterable: false,
            initial,
            detail_height: 4,
        };
        let ListResult::Selected(r) = list_screen(term, spec)? else {
            return Ok(None);
        };
        match actions[r].take() {
            Some(TargetRow::Disk(d)) => return Ok(Some(d)),
            Some(TargetRow::Partition) => {
                if let Some(d) = pick_disk_to_partition(term, &disks)? {
                    devices::partition(&d)?;
                    term.clear()?;
                    remembered = Some(d);
                }
            }
            _ => {}
        }
    }
}

fn pick_disk_to_partition<B: Backend>(
    term: &mut Terminal<B>,
    disks: &[WriteTarget],
) -> io::Result<Option<WriteTarget>> {
    let disks: Vec<&WriteTarget> = disks.iter().filter(|d| !devices::is_floppy(d)).collect();
    let rows = disks
        .iter()
        .map(|d| Row::Item {
            cols: vec![
                d.name.clone(),
                devices::size_label(d),
                devices::bus(d).into(),
                d.model.to_string(),
            ],
            disabled: false,
            detail: vec![],
            search: d.name.clone(),
        })
        .collect();
    let spec = ListSpec {
        chrome: Chrome {
            step: Chrome::step(2, "Target"),
            heading: "Partition which disk?",
            heading_right: String::new(),
            keys: "Up/Down move   Enter open cfdisk   Esc back",
        },
        layout: vec![Col::Left(8), Col::Right(10), Col::Left(7), Col::Flex],
        rows,
        filterable: false,
        initial: None,
        detail_height: 0,
    };
    Ok(match list_screen(term, spec)? {
        ListResult::Selected(i) => Some(disks[i].clone()),
        ListResult::Back => None,
    })
}

fn pick_method<B: Backend>(
    term: &mut Terminal<B>,
    image: &CatalogImage,
    target: &WriteTarget,
    previous: Option<Method>,
) -> io::Result<Option<Method>> {
    let default = Method::default_for(image, target);
    let mut rows = Vec::new();
    for m in Method::ALL {
        let why = m.unavailable(image, target);
        let note = match (why, m == default) {
            (Some(why), _) => why.to_string(),
            (None, true) => "recommended".into(),
            _ => String::new(),
        };
        let mut detail = vec![field(m.name(), m.summary().into())];
        detail.push(match m {
            Method::Raw if image.kind == "cd-only" => Line::styled(
                "            This is a CD-only image: written raw, the disk won't boot.",
                ui::warn_style(),
            ),
            Method::Raw => field_more(format!("the result boots as {}", m.boot_hint(image, target))),
            Method::UsbHdd | Method::UsbZip => field_more(format!(
                "an 8 MB disk the server builds; set the BIOS to boot {}",
                m.boot_hint(image, target)
            )),
            Method::Uefi => field_more(match image.variant("uefi").and_then(|v| v.size) {
                Some(size) => format!("ready on the server ({}); boots {}", bytesize::ByteSize::b(size), m.boot_hint(image, target)),
                None => "the server builds it first (a minute or two); Windows: install.wim split, BIOS boot too".into(),
            }),
            Method::FloppySet => field_more(format!(
                "disks {}..{} of {}; you're asked to insert each one",
                image.set_index.unwrap_or(1),
                image.set_size.unwrap_or(1),
                image.set_size.unwrap_or(1)
            )),
            _ => field_more("not built yet".into()),
        });
        rows.push(Row::Item {
            cols: vec![m.name().into(), note],
            disabled: why.is_some(),
            detail,
            search: m.name().into(),
        });
    }
    let initial = Method::ALL
        .iter()
        .position(|&m| Some(m) == previous)
        .or_else(|| Method::ALL.iter().position(|&m| m == default));
    let spec = ListSpec {
        chrome: Chrome {
            step: Chrome::step(3, "Method"),
            heading: "How should it be written?",
            heading_right: String::new(),
            keys: "Up/Down move   Enter select   Esc back",
        },
        layout: vec![Col::Left(28), Col::Flex],
        rows,
        filterable: false,
        initial,
        detail_height: 3,
    };
    Ok(match list_screen(term, spec)? {
        ListResult::Selected(i) => Some(Method::ALL[i]),
        ListResult::Back => None,
    })
}

/// The overview. True = write.
fn review<B: Backend>(
    term: &mut Terminal<B>,
    image: &CatalogImage,
    plan: &Plan,
) -> io::Result<bool> {
    let t = &plan.target;
    let mut body = vec![
        field("Image", image.name.clone()),
        field_more(format!(
            "{}  |  {}  |  {}",
            image.section,
            image.kind_label(),
            image.size_label()
        )),
        Line::raw(""),
        field(
            "Target",
            format!(
                "{}  {}  {}",
                t.devnode.to_string_lossy(),
                devices::size_label(t),
                devices::bus(t)
            ),
        ),
        field_more(t.model.to_string()),
        Line::raw(""),
        field("Method", plan.method.name().into()),
    ];
    match plan.method {
        Method::FloppySet => body.push(field_more(format!(
            "{} disks, each verified; you swap floppies in between",
            plan.disks.len()
        ))),
        _ => body.push(field_more(plan.method.summary().into())),
    }
    body.push(Line::raw(""));
    if image.kind == "cd-only" && plan.method == Method::Raw {
        body.push(Line::styled(
            "CD-only image: the written disk will NOT boot, it only boots from a CD drive.",
            ui::warn_style(),
        ));
    } else {
        let hint = if plan.method == Method::Uefi {
            plan.disks[0].boot_hint.clone()
        } else {
            plan.method.boot_hint(image, t)
        };
        body.push(field("Next boot", format!("boot it as {hint}")));
    }
    body.push(Line::raw(""));
    body.push(Line::styled(
        format!(
            "All data on {} will be erased.",
            t.devnode.to_string_lossy()
        ),
        ui::danger_style(),
    ));
    let chrome = Chrome {
        step: Chrome::step(4, "Review"),
        heading: "Ready to write",
        heading_right: String::new(),
        keys: "Left/Right choose   Enter confirm   Esc back",
    };
    // focus starts on "Back": writing needs a deliberate move to "Write"
    Ok(dialog(term, chrome, body, &["Back", "Write"], 0)? == Some(1))
}

/// Between disks of a floppy set. True = continue with `disk`.
pub fn insert_disk<B: Backend>(term: &mut Terminal<B>, plan: &Plan, n: usize) -> io::Result<bool> {
    let disk = &plan.disks[n];
    let body = vec![
        field(
            "Next",
            format!("{}  ({} of {})", disk.name, n + 1, plan.disks.len()),
        ),
        Line::raw(""),
        Line::raw(format!(
            "Take the written floppy out of {}, label it, and insert a blank one.",
            plan.target.devnode.to_string_lossy()
        )),
        Line::raw(""),
        Line::styled(
            "Everything on the inserted floppy will be erased.",
            ui::danger_style(),
        ),
    ];
    let chrome = Chrome {
        step: String::from("Floppy set"),
        heading: "Insert the next floppy",
        heading_right: String::new(),
        keys: "Left/Right choose   Enter confirm   Esc stop",
    };
    Ok(dialog(term, chrome, body, &["Write it", "Stop here"], 0)? == Some(0))
}

/// Wait for the server to build the UEFI stick image. Returns its size and boot hint,
/// or None if the user backed out (or it failed, after saying so).
fn prepare_variant<B: Backend>(
    term: &mut Terminal<B>,
    image: &CatalogImage,
    target: &WriteTarget,
) -> anyhow::Result<Option<(u64, String)>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    let v = image.variant("uefi").expect("checked by unavailable()");
    let status_url = v
        .status_url
        .clone()
        .expect("uefi variants have a status URL");
    let started = std::time::Instant::now();
    loop {
        let st = catalog::VariantStatus::fetch(&status_url)?;
        match st.state.as_str() {
            "ready" => {
                let size = st.size.unwrap_or(0);
                if devices::capacity(target).is_some_and(|c| c < size) {
                    notice(
                        term,
                        "The target is too small",
                        vec![
                            format!(
                                "The UEFI stick for {} needs {}.",
                                image.name,
                                bytesize::ByteSize::b(size)
                            ),
                            format!(
                                "{} only has {}.",
                                target.devnode.to_string_lossy(),
                                devices::size_label(target)
                            ),
                        ],
                    )?;
                    return Ok(None);
                }
                return Ok(Some((
                    size,
                    st.boot_hint.unwrap_or_else(|| v.boot_hint.clone()),
                )));
            }
            "error" => {
                notice(
                    term,
                    "The server couldn't build the UEFI stick",
                    vec![st.error.unwrap_or_default()],
                )?;
                return Ok(None);
            }
            _ => {}
        }
        let elapsed = started.elapsed().as_secs();
        let body = vec![
            field("Image", image.name.clone()),
            Line::raw(""),
            Line::raw("The server is building the UEFI stick image: it extracts the ISO,"),
            Line::raw("splits a large install.wim and copies everything onto FAT32."),
            Line::raw(""),
            field("Step", st.step.unwrap_or_else(|| st.state.clone())),
            field("Waiting", format!("{}:{:02}", elapsed / 60, elapsed % 60)),
        ];
        let chrome = Chrome {
            step: Chrome::step(3, "Method"),
            heading: "Preparing the image",
            heading_right: String::new(),
            keys: "Esc back (the server keeps building)",
        };
        term.draw(|f| ui::draw_static(f, &chrome, &body))?;
        // poll every 2s, but react to Esc right away
        if event::poll(std::time::Duration::from_secs(2))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
            && k.code == KeyCode::Esc
        {
            return Ok(None);
        }
    }
}

/// Before writing to a floppy drive: wait for a readable disk in it. False = stop.
pub fn ready_floppy<B: Backend>(term: &mut Terminal<B>, target: &WriteTarget) -> io::Result<bool> {
    if !devices::is_floppy(target) {
        return Ok(true);
    }
    loop {
        let Err(e) = devices::settle_floppy(target) else {
            return Ok(true);
        };
        let body = vec![
            Line::raw(format!(
                "{} can't be read: {e}",
                target.devnode.to_string_lossy()
            )),
            Line::raw(""),
            Line::raw("Is a disk in the drive? Brand-new floppies may also be unformatted;"),
            Line::raw("those have to be formatted before an image can be written to them."),
        ];
        let chrome = Chrome {
            step: String::from("Floppy"),
            heading: "No readable floppy",
            heading_right: String::new(),
            keys: "Left/Right choose   Enter confirm   Esc stop",
        };
        if dialog(term, chrome, body, &["Retry", "Stop"], 0)? != Some(0) {
            return Ok(false);
        }
    }
}

/// Error or notice screen with a single button.
pub fn notice<B: Backend>(
    term: &mut Terminal<B>,
    heading: &str,
    lines: Vec<String>,
) -> io::Result<()> {
    let chrome = Chrome {
        step: String::new(),
        heading,
        heading_right: String::new(),
        keys: "Enter continue",
    };
    let body = lines.into_iter().map(Line::raw).collect();
    dialog(term, chrome, body, &["OK"], 0).map(|_| ())
}
