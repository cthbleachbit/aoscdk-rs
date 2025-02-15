use crate::{
    disks::{self, mbr_is_primary_partition, ALLOWED_FS_TYPE},
    install::{self, umount_all},
    log::setup_logger,
    network::{self, Mirror, VariantEntry},
};
use anyhow::Result;
use cursive::{
    event::Event,
    views::{
        Dialog, DummyView, EditView, LinearLayout, ListView, NamedView, Panel, ProgressBar,
        RadioGroup, ResizedView, ScrollView, SelectView, TextContent, TextView,
    },
};
use cursive::{traits::*, utils::Counter};
use cursive::{view::SizeConstraint, views::Button};
use cursive::{Cursive, View};
use cursive_async_view::AsyncView;
use cursive_table_view::{TableView, TableViewItem};
use log::{error, info};
use number_prefix::NumberPrefix;
use std::{cell::RefCell, sync::Arc, thread};
use std::{env, fs, io::Read, path::PathBuf};
use std::{os::unix::prelude::AsRawFd, rc::Rc};
use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{
    begin_install, games::add_main_callback, AtomicBoolWrapper, InstallConfig, DEFAULT_EMPTY_SIZE,
};

const LAST_USER_CONFIG_FILE: &str = "/tmp/deploykit-config.json";
const SAVE_USER_CONFIG_FILE: &str = "/root/deploykit-config.json";

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum VariantColumn {
    Name,
    Date,
    Size,
}

impl TableViewItem<VariantColumn> for network::VariantEntry {
    fn to_column(&self, column: VariantColumn) -> String {
        match column {
            VariantColumn::Name => self.name.clone(),
            VariantColumn::Date => self.date.clone(),
            VariantColumn::Size => human_size(self.size),
        }
    }
    fn cmp(&self, other: &Self, column: VariantColumn) -> std::cmp::Ordering
    where
        Self: Sized,
    {
        match column {
            VariantColumn::Name => self.name.cmp(&other.name),
            VariantColumn::Date => self.date.cmp(&other.date),
            VariantColumn::Size => self.size.cmp(&other.size),
        }
    }
}

macro_rules! SUMMARY_TEXT {
    () => {
        "Installer will perform the following operations:\n- {} will be erased and formatted as {}.\n- AOSC OS {} will be downloaded from {}.\n- User {} will be created.\n- AOSC OS will use the {} locale.\n- Your timezone will be set to {}, and will use {} as local time.\n"
    };
}

macro_rules! SURE_FS_TYPE_INFO {
    () => {
        "AOSC OS Installation has detected that the specified partition is currently formatted {}, would you like to format this partition using the original filesystem? For its proven reliability, we recommend formatting your system partition as ext4."
    };
}

macro_rules! SURE_FS_FORMAT_INFO {
    () => {
        "Installer has detected an existing file system on the specified partition, {}. Please consider verifying if there is data in this partition that is yet to be backed up.\n\nAfter the final confirmation, coming up in a few steps, Installer will format this partition as {}. "
    };
}

const ADVANCED_METHOD_INFO: &str = "Installer detected an unsupported filesystem format in your system partition. If you proceed, the installer will format your system partition using the ext4 filesystem. Please refer to the manual installation guides if you prefer to use an unsupported filesystem.";
const WELCOME_TEXT: &str = r#"Welcome to the AOSC OS Installer!

In the following pages, Installer will guide you through the variant selection, partitioning, and other installation steps. The installation process should only take a few minutes, but will require more time on slower hardware."#;
const VARIANT_TEXT: &str =
    "Shown below is a list of available AOSC OS distributions for your device.";
const ENTER_USER_PASSWORD_TEXT: &str = r#"Please enter and confirm your desired username and password. Please note that your username must start with a lower-cased alphabetical letter (a-z), and contain only lower-cased letters a-z, numbers 0-9, and dash ("-").
"#;
const ENTER_HOSTNAME_TEXT: &str = r#"Now, please input your desired hostname. A hostname may only consist letters a-z, numbers 0-9, and dash ("-")."#;
const ENTER_TIMEZONE_TEXT: &str = r#"Finally, please select your locale, timezone, and your clock preferences. Your locale setting will affect your installation's display language. UTC system time is the default setting for Linux systems, but may result in time discrepancy with your other operating systems, such as Windows. If you wish to prevent this from happening, please select local time as system time."#;
const BENCHMARK_TEXT: &str = "Installer will now test all mirrors for download speed, and rank them from the fastest (top) to the slowest (bottom). This may take a few minutes.";
const FINISHED_TEXT: &str = r#"AOSC OS has been successfully installed on your device.

You may reboot to your installed system by choosing "Reboot," or return to LiveKit by selecting "Exit to LiveKit.""#;

macro_rules! fill_in_all_the_fields {
    ($s:ident) => {
        show_msg($s, "Please fill in all the fields.");
        return;
    };
}

macro_rules! show_fetch_progress {
    ($siv:ident, $m:tt, $e:tt, $f:block) => {{
        $siv.pop_layer();
        $siv.add_layer(
            Dialog::around(TextView::new(format!(
                "{}\nThis may take a few minutes ...",
                $m
            )))
            .title("Installation Progress"),
        );
        $siv.refresh();
        let ret = { $f };
        if ret.is_err() {
            show_error($siv, $e);
            return;
        }
        $siv.pop_layer();
        ret.unwrap()
    }};
    ($siv:ident, $m:tt, $f:block) => {{
        $siv.pop_layer();
        $siv.add_layer(
            Dialog::around(TextView::new(format!(
                "{}\nThis may take a few minutes ...",
                $m
            )))
            .title("Installation Progress"),
        );
        // $siv.refresh();
        let ret = { $f };
        $siv.pop_layer();
        ret
    }};
}

type PartitionButton = (&'static str, &'static dyn Fn(&mut Cursive, InstallConfig));

fn show_error(siv: &mut Cursive, msg: &str) {
    siv.add_layer(
        Dialog::around(TextView::new(msg).max_width(80))
            .title("Error")
            .button("Exit", |s| s.quit())
            .padding_lrtb(2, 2, 1, 1),
    );
}

fn show_msg(siv: &mut Cursive, msg: &str) {
    siv.add_layer(
        Dialog::around(TextView::new(msg).max_width(80))
            .title("AOSC OS Installer")
            .button("OK", |s| {
                s.pop_layer();
            })
            .padding_lrtb(2, 2, 1, 1),
    );
}

fn show_blocking_message(siv: &mut Cursive, msg: &str) {
    siv.add_layer(
        Dialog::around(TextView::new(msg))
            .title("AOSC OS Installer")
            .padding_lrtb(2, 2, 1, 1),
    );
}

fn partition_button() -> PartitionButton {
    if env::var("DISPLAY").is_ok() {
        return ("Open GParted", &|s, _| {
            show_blocking_message(s, "Waiting for GParted Partitioning Program to exit ...");
            let cb_sink = s.cb_sink().clone();
            thread::spawn(move || {
                Command::new("gparted").output().ok();
                cb_sink
                    .send(Box::new(|s| {
                        let new_parts = disks::list_partitions();
                        let (disk_list, disk_view) = make_partition_list(new_parts);
                        s.set_user_data(disk_list);
                        s.call_on_name("part_list", |view: &mut NamedView<LinearLayout>| {
                            *view = disk_view;
                        });
                        s.pop_layer();
                    }))
                    .unwrap();
            });
        });
    }

    ("Open Shell", &|s, config| {
        s.set_user_data(config);
        let dump = s.dump();
        s.quit();
        s.set_user_data(dump);
    })
}

#[inline]
fn human_size(size: u64) -> String {
    match NumberPrefix::binary(size as f64) {
        NumberPrefix::Standalone(bytes) => format!("{bytes} B"),
        NumberPrefix::Prefixed(prefix, n) => format!("{n:.1} {prefix}B"),
    }
}

fn make_partition_list(
    partitions: Vec<disks::Partition>,
) -> (RadioGroup<disks::Partition>, NamedView<LinearLayout>) {
    let mut disk_view = LinearLayout::vertical();
    let mut disk_list = RadioGroup::new();
    for part in &partitions {
        let path_name = if let Some(path) = &part.path {
            path.to_string_lossy().to_string()
        } else {
            "?".to_owned()
        };
        let radio = disk_list.button(
            part.clone(),
            format!(
                "{} ({}, {})",
                path_name,
                part.fs_type
                    .as_ref()
                    .unwrap_or(&"Unknown/Unformatted".to_owned()),
                human_size(part.size)
            ),
        );
        disk_view.add_child(radio);
    }
    if partitions.is_empty() {
        let dummy_partition = disks::Partition {
            path: None,
            parent_path: None,
            fs_type: None,
            size: 0,
        };
        disk_view.add_child(disk_list.button(
            dummy_partition,
            "Please select a system partition for AOSC OS.",
        ));
    }

    (disk_list, disk_view.with_name("part_list"))
}

pub fn wrap_in_dialog<V: View, S: Into<String>>(
    inner: V,
    title: S,
    width: Option<usize>,
) -> Dialog {
    Dialog::around(ResizedView::new(
        SizeConstraint::AtMost(width.unwrap_or(64)),
        SizeConstraint::Free,
        ScrollView::new(inner),
    ))
    .padding_lrtb(2, 2, 1, 1)
    .title(title)
}

fn build_variant_list(
    mirrors: Vec<Mirror>,
    variants: Vec<VariantEntry>,
    config: InstallConfig,
) -> Dialog {
    let mut config_view = LinearLayout::vertical();

    let variant_view = TableView::<network::VariantEntry, VariantColumn>::new()
        .column(VariantColumn::Name, "Available Distributions", |c| {
            c.width(30)
        })
        .column(VariantColumn::Date, "Last Updated", |c| c.width(22))
        .column(VariantColumn::Size, "Download Size", |c| c.width(22))
        .items(variants.clone())
        .on_submit(move |siv, _row, index| {
            let mut config = config.clone();
            config.variant = Some(Arc::new(variants.get(index).unwrap().clone()));
            select_mirrors(siv, mirrors.clone(), config);
        })
        .min_width(80)
        .min_height(30);
    let variant_view = Panel::new(variant_view).title("Variant");
    config_view.add_child(TextView::new(VARIANT_TEXT));
    config_view.add_child(variant_view);
    config_view.add_child(DummyView {});

    wrap_in_dialog(config_view, "AOSC OS Installation", Some(128)).button("Exit", |s| s.quit())
}

fn select_variant(siv: &mut Cursive, config: InstallConfig) {
    siv.pop_layer();
    let loader = AsyncView::new_with_bg_creator(
        siv,
        move || {
            let manifest = network::fetch_recipe().map_err(|e| e.to_string())?;
            let mirrors = network::fetch_mirrors(&manifest);
            let variants = network::find_variant_candidates(manifest).map_err(|e| e.to_string())?;
            Ok((mirrors, variants))
        },
        move |(mirrors, variants)| build_variant_list(mirrors, variants, config.clone()),
    );

    siv.add_layer(loader);
}

fn select_mirrors(siv: &mut Cursive, mirrors: Vec<Mirror>, config: InstallConfig) {
    siv.pop_layer();
    let (config_view, repo_list) = select_mirror_view_base(&mirrors);
    siv.add_layer(select_mirrors_view(config_view, config, repo_list, mirrors));
}

fn select_mirror_view_base(mirrors: &[Mirror]) -> (LinearLayout, RadioGroup<Mirror>) {
    let mut config_view = LinearLayout::vertical();
    let mut repo_list = RadioGroup::new();
    let mirror_list = mirrors;
    let mut repo_view = LinearLayout::vertical()
        .child(TextView::new(
            "Please select a mirror to download AOSC OS. Generally, a mirror closest to you geographically would be the best bet for download speeds.",
        ))
        .child(DummyView {});
    for mirror in mirror_list {
        let radio = repo_list.button(mirror.clone(), format!("{} ({})", mirror.name, mirror.loc));
        repo_view.add_child(radio);
    }
    let repo_view = Panel::new(repo_view).title("Mirrors");
    config_view.add_child(repo_view);
    config_view.add_child(DummyView {});

    (config_view, repo_list)
}

fn select_mirrors_view(
    config_view: LinearLayout,
    config: InstallConfig,
    repo_list: RadioGroup<Mirror>,
    mirrors: Vec<Mirror>,
) -> Dialog {
    let config_clone = config.clone();
    let config_clone_2 = config.clone();
    wrap_in_dialog(config_view, "AOSC OS Installation", None)
        .button("Continue", move |s| {
            let mut config = config.clone();
            let mirror = repo_list.selection();
            config.mirror = Some(Arc::new(Rc::as_ref(&mirror).clone()));
            if config.partition.is_some() {
                select_user_password(s, config);
            } else {
                select_partition(s, config);
            }
        })
        .button("Benchmark Mirrors", move |s| {
            let config_clone_2 = config_clone.clone();
            let config_clone_3 = config_clone.clone();
            let mirrors_clone = mirrors.clone();
            let mirrors_clone_2 = mirrors.clone();
            s.pop_layer();
            s.add_layer(
                Dialog::around(TextView::new(BENCHMARK_TEXT).max_width(80))
                    .title("AOSC OS Installer")
                    .button("OK", move |s| {
                        let config_clone_3 = config_clone_2.clone();
                        let mirrors_clone_2 = mirrors_clone.clone();
                        let loader = AsyncView::new_with_bg_creator(
                            s,
                            move || {
                                let new_mirrors = network::speedtest_mirrors(mirrors_clone_2);
                                Ok(new_mirrors)
                            },
                            move |mirrors| {
                                let (config_view, repo_list) = select_mirror_view_base(&mirrors);
                                select_mirrors_view(
                                    config_view,
                                    config_clone_3.clone(),
                                    repo_list,
                                    mirrors,
                                )
                            },
                        );
                        s.pop_layer();
                        s.add_layer(loader);
                    })
                    .button("Cancel", move |s| {
                        let mirrors_clone_3 = mirrors_clone_2.clone();
                        let config_clone_4 = config_clone_3.clone();
                        s.pop_layer();
                        select_mirrors(s, mirrors_clone_3, config_clone_4);
                    })
                    .padding_lrtb(2, 2, 1, 1),
            );
        })
        .button("Back", move |s| {
            s.pop_layer();
            select_variant(s, config_clone_2.clone());
        })
        .button("Exit", |s| s.quit())
}

fn select_partition(siv: &mut Cursive, config: InstallConfig) {
    let partitions = show_fetch_progress!(siv, "Probing disks ...", { disks::list_partitions() });
    let (disk_list, disk_view) = make_partition_list(partitions);
    siv.set_user_data(disk_list);
    let dest_view = LinearLayout::vertical()
    .child(TextView::new(
        "Please select a partition as AOSC OS system partition. If you would like to make changes to your partitions, please select \"Open GParted.\"",
    ))
    .child(DummyView {})
    .child(disk_view);
    let config_view = LinearLayout::vertical()
        .child(Panel::new(dest_view).title("Select System Partition"))
        .child(DummyView {});
    let (btn_label, btn_cb) = partition_button();
    let config_copy = config.clone();
    let config_copy_2 = config.clone();
    let config_clone_3 = config.clone();
    siv.add_layer(
        wrap_in_dialog(config_view, "AOSC OS Installation", None)
        .button("Continue", move |s| {
            let disk_list = s.user_data::<RadioGroup<disks::Partition>>();
            let required_size = config_clone_3.variant.as_ref().unwrap().install_size;
            if let Some(disk_list) = disk_list {
                let disk_list = disk_list.clone();
                let current_partition = if cfg!(debug_assertions) {
                    // prevent developer/tester accidentally delete their partitions
                    Rc::new(disks::Partition {
                        fs_type: None,
                        path: Some(PathBuf::from("/dev/loop0p1")),
                        parent_path: Some(PathBuf::from("/dev/loop0")),
                        size: required_size + 10 * 1024 * 1024 * 1024,
                    })
                } else {
                    disk_list.selection()
                };
                if current_partition.parent_path.is_none() && current_partition.size == 0 {
                    show_msg(s, "Please specify a system partition.");
                    // s.refresh();
                    return;
                }
                if current_partition.size < required_size {
                    show_msg(
                        s,
                        &format!(
                            "The specified partition does not contain enough space to install AOSC OS release!\n\nAvailable space: {:.3}GiB\nRequired space: {:.3}GiB", 
                            current_partition.size as f32 / 1024.0 / 1024.0 / 1024.0,
                            required_size as f32 / 1024.0 / 1024.0 / 1024.0
                        ));
                    return;
                }
                let mut config = config.clone();
                let config_copy = config.clone();
                let config_copy_2 = config.clone();
                let fs_type = current_partition.fs_type.clone();
                let current_partition_clone = current_partition.clone();
                if let Err(e) = mbr_is_primary_partition(current_partition.parent_path.as_deref()) {
                    show_msg(s, &e.to_string());
                }
                if let Err(e) = disks::right_combine(current_partition.parent_path.as_deref()) {
                    let view = wrap_in_dialog(LinearLayout::vertical()
                    .child(TextView::new(e.to_string())), "AOSC OS Installer", None)
                    .button("OK", |s| {
                        s.pop_layer();
                    })
                    .button("Exit", |s| s.quit());
                    s.add_layer(view);
                    return;
                }
                if let Some(fs_type) = fs_type {
                    if fs_type != "ext4" && ALLOWED_FS_TYPE.contains(&fs_type.as_str()) {
                        let view = wrap_in_dialog(LinearLayout::vertical()
                        .child(TextView::new(format!(SURE_FS_TYPE_INFO!(), &fs_type))), "AOSC OS Installer", None)
                        .button(format!("Use {fs_type}"), move |s| {
                            let new_part = disks::fill_fs_type(current_partition_clone.as_ref(), false);
                            let mut config_clone = config_copy.clone();
                            config_clone.partition = Some(Arc::new(new_part.clone()));
                            s.pop_layer();
                            continue_to_format_hdd(s, config_clone, new_part.fs_type.expect("Must unwrap success"));
                        })
                        .button("Use Ext4", move |s| {
                            let new_part = disks::fill_fs_type(current_partition.as_ref(), true);
                            let mut config_clone = config_copy_2.clone();
                            config_clone.partition = Some(Arc::new(new_part.clone()));
                            continue_to_format_hdd(s, config_clone, new_part.fs_type.expect("Must unwrap success"));
                        })
                        .button("Cancel", move |s| {
                            s.cb_sink()
                            .send(Box::new(|s| {
                                s.pop_layer();
                            }))
                            .unwrap()
                        });
                        s.add_layer(view);
                    } else if fs_type == "ext4" {
                        let new_part = disks::fill_fs_type(current_partition_clone.as_ref(), true);
                        config.partition = Some(Arc::new(new_part.clone()));
                        continue_to_format_hdd(s, config, new_part.fs_type.expect("Must unwrap success"));
                    } else if !ALLOWED_FS_TYPE.contains(&fs_type.as_str()) {
                        let view = wrap_in_dialog(LinearLayout::vertical()
                        .child(TextView::new(ADVANCED_METHOD_INFO)), "AOSC OS Installer", None)
                        .button("OK", move |s| {
                            let new_part = disks::fill_fs_type(current_partition_clone.as_ref(), true);
                            let mut config_clone = config_copy.clone();
                            config_clone.partition = Some(Arc::new(new_part.clone()));
                            s.pop_layer();
                            continue_to_format_hdd(s, config_clone, new_part.fs_type.expect("Must unwrap success"));
                        })
                        .button("Cancel", move |s| {
                            s.cb_sink()
                            .send(Box::new(|s| {
                                s.pop_layer();
                            }))
                            .unwrap()
                        });
                        s.add_layer(view);
                    }
                } else {
                    let new_part = disks::fill_fs_type(current_partition_clone.as_ref(), true);
                    config.partition = Some(Arc::new(new_part.clone()));
                    continue_to_format_hdd(s, config, new_part.fs_type.expect("Must success unwrap"));
                }
            }
        })
        .button(btn_label, move |s| {
            btn_cb(s, config_copy.clone());
        })
        .button("Back", move |s| {
            s.pop_layer();
            select_variant(s, config_copy_2.clone());
        })
        .button("Exit", |s| s.quit())
    );
}

fn continue_to_format_hdd(s: &mut Cursive, config_clone: InstallConfig, fs_type: String) {
    let path = config_clone
        .partition
        .as_ref()
        .expect("Must unwrap success")
        .path
        .as_ref()
        .expect("Must unwrap success")
        .to_str()
        .expect("Must as string");

    let dialog = LinearLayout::vertical().child(TextView::new(format!(
        SURE_FS_FORMAT_INFO!(),
        path, fs_type
    )));

    let view = wrap_in_dialog(dialog, "AOSC OS Installer", None)
        .button("OK", move |s| {
            partition_view_to_next(s, config_clone.clone())
        })
        .button("Cancel", move |s| {
            s.cb_sink()
                .send(Box::new(|s| {
                    s.pop_layer();
                }))
                .unwrap()
        });

    s.add_layer(view);
}

fn partition_view_to_next(s: &mut Cursive, config_clone: InstallConfig) {
    s.pop_layer();
    if config_clone.user.is_some() {
        is_use_last_config(s, config_clone);
    } else {
        select_user_password(s, config_clone);
    }
}

fn select_user_password(siv: &mut Cursive, config: InstallConfig) {
    siv.pop_layer();
    let password = Rc::new(RefCell::new(String::new()));
    let password_copy = Rc::clone(&password);
    let password_confirm = Rc::new(RefCell::new(String::new()));
    let password_confirm_copy = Rc::clone(&password_confirm);
    let name = Rc::new(RefCell::new(String::new()));
    let name_copy = Rc::clone(&name);

    let user_password_textview = TextView::new(ENTER_USER_PASSWORD_TEXT).max_width(80);
    let user_password_view = ListView::new()
        .child(
            "Username",
            EditView::new()
                .on_edit_mut(move |_, c, _| {
                    name_copy.replace(c.to_owned());
                })
                .min_width(20)
                .with_name("user"),
        )
        .child(
            "Password",
            EditView::new()
                .secret()
                .on_edit_mut(move |_, c, _| {
                    password_copy.replace(c.to_owned());
                })
                .min_width(20)
                .with_name("pwd"),
        )
        .child(
            "Confirm Password",
            EditView::new()
                .secret()
                .on_edit_mut(move |_, c, _| {
                    password_confirm_copy.replace(c.to_owned());
                })
                .min_width(20)
                .with_name("pwd2"),
        );
    let config_clone = config.clone();
    let user_password_dialog = wrap_in_dialog(
        LinearLayout::vertical()
            .child(user_password_textview)
            .child(DummyView {})
            .child(user_password_view),
        "AOSC OS Installer",
        None,
    )
    .button("Continue", move |s| {
        let password = password.as_ref().to_owned().into_inner();
        let password_confirm = password_confirm.as_ref().to_owned().into_inner();
        let name = name.as_ref().to_owned().into_inner();
        if !install::is_acceptable_username(&name) {
            show_msg(s, "Username is not valid, please refer to the criteria specified on top of the dialog.");
            return;
        }
        if password.is_empty() || password_confirm.is_empty() || name.is_empty() {
            fill_in_all_the_fields!(s);
        }
        if password != password_confirm {
            show_msg(s, "Passwords password do not match.");
            return;
        }
        let mut config = config.clone();
        config.password = Some(Arc::new(password));
        config.user = Some(Arc::new(name));
        select_hostname(s, config);
    })
    .button("Back", move |s| {
        s.pop_layer();
        select_partition(s, config_clone.clone());
    })
    .button("Exit", |s| s.quit());

    siv.add_layer(user_password_dialog);
}

fn select_hostname(siv: &mut Cursive, config: InstallConfig) {
    siv.pop_layer();
    let hostname = Rc::new(RefCell::new(String::new()));
    let hostname_copy = Rc::clone(&hostname);
    let hostname_textview = TextView::new(ENTER_HOSTNAME_TEXT);
    let hostname_view = ListView::new()
        .child(
            "Hostname",
            EditView::new()
                .on_edit_mut(move |_, c, _| {
                    hostname_copy.replace(c.to_owned());
                })
                .min_width(20)
                .with_name("hostname"),
        )
        .delimiter();
    let config_clone = config.clone();
    let hostname_dialog = wrap_in_dialog(
        LinearLayout::vertical()
            .child(hostname_textview)
            .child(DummyView {})
            .child(hostname_view),
        "AOSC OS Installer",
        None,
    )
    .button("Continue", move |s| {
        let hostname = hostname.as_ref().to_owned().into_inner();
        if hostname.is_empty() {
            fill_in_all_the_fields!(s);
        }
        if !install::is_valid_hostname(&hostname) {
            show_msg(s, "Hostname is not vaild!");
            return;
        }
        let mut config = config.clone();
        config.hostname = Some(hostname);
        select_timezone(s, config);
    })
    .button("Back", move |s| {
        s.pop_layer();
        select_user_password(s, config_clone.clone());
    })
    .button("Exit", |s| s.quit());

    siv.add_layer(hostname_dialog);
}

fn select_timezone(siv: &mut Cursive, config: InstallConfig) {
    siv.pop_layer();
    // locale default is C.UTF-8
    let locale = Rc::new(RefCell::new(String::from("C.UTF-8")));
    let locale_copy = Rc::clone(&locale);
    let timezone = Rc::new(RefCell::new(String::from("UTC")));
    let timezone_copy = Rc::clone(&timezone);
    // RTC/UTC default is UTC
    let tc = Rc::new(RefCell::new(String::from("UTC")));
    let tc_copy = Rc::clone(&tc);
    let locales = install::get_locale_list().unwrap();
    let timezone_textview = TextView::new(ENTER_TIMEZONE_TEXT);
    let mut timezone_selected_status = TextView::new("UTC");
    let timezone_status_text = Arc::new(timezone_selected_status.get_shared_content());
    let mut locale_selected_status = TextView::new("C.UTF-8");
    let locale_status_text = Arc::new(locale_selected_status.get_shared_content());

    let timezone_view = ListView::new()
        .child(
            "Timezone",
            Button::new("Select timezone", move |s| {
                let zoneinfo = install::get_zoneinfo_list().unwrap();
                s.add_layer(set_timezone(
                    zoneinfo,
                    timezone_copy.clone(),
                    timezone_status_text.clone(),
                ))
            }),
        )
        .child("Selected Timezone", timezone_selected_status.center())
        .child(
            "Locale",
            Button::new("Select locale", move |s| {
                s.add_layer(set_locales(
                    locales.clone(),
                    locale_copy.clone(),
                    locale_status_text.clone(),
                ))
            }),
        )
        .child("Selected locale", locale_selected_status.center())
        .child(
            "RTC Timezone",
            SelectView::new()
                .autojump()
                .popup()
                .with_all_str(vec!["UTC (Recommended)", "Local time (like Windows)"])
                .on_submit(move |_, c: &str| {
                    let selected = match c {
                        "UTC (Recommended)" => "UTC",
                        "Local time (like Windows)" => "RTC",
                        _ => unreachable!(),
                    };
                    tc_copy.replace(selected.to_string());
                })
                .min_width(20),
        );
    let config_clone = config.clone();
    let timezone_dialog = wrap_in_dialog(
        LinearLayout::vertical()
            .child(timezone_textview)
            .child(DummyView {})
            .child(timezone_view),
        "AOSC OS Installer",
        None,
    )
    .button("Continue", move |s| {
        let locale = locale.as_ref().to_owned().into_inner();
        let timezone = timezone.as_ref().to_owned().into_inner();
        let tc = tc.as_ref().to_owned().into_inner();
        if locale.is_empty() || timezone.is_empty() || tc.is_empty() {
            fill_in_all_the_fields!(s);
        }
        let mut config = config.clone();
        config.locale = Some(Arc::new(locale));
        config.timezone = Some(Arc::new(timezone));
        config.tc = Some(Arc::new(tc));
        // show_summary(s, config);
        select_swap(s, config);
    })
    .button("Back", move |s| {
        s.pop_layer();
        select_hostname(s, config_clone.clone());
    })
    .button("Exit", |s| s.quit());

    siv.add_layer(timezone_dialog);
}

// Filter cities with names containing query string. You can implement your own logic here!
fn search_fn<T: std::iter::IntoIterator<Item = String>>(items: T, query: &str) -> Vec<String> {
    items
        .into_iter()
        .filter(|item| {
            let item = item.to_lowercase();
            let query = query.to_lowercase();
            item.contains(&query)
        })
        .collect()
}

fn replace_item(
    siv: &mut Cursive,
    item: &str,
    timezone: Rc<RefCell<String>>,
    status_text: Arc<TextContent>,
) {
    siv.pop_layer();
    timezone.replace(item.to_string());
    status_text.set_content(item);
}

fn on_submit(
    siv: &mut Cursive,
    query: &str,
    timezone_clone: Rc<RefCell<String>>,
    status_text: Arc<TextContent>,
) {
    let matches = siv.find_name::<SelectView>("matches").unwrap();
    if matches.is_empty() {
        // not all people live in big cities. If none of the cities in the list matches, use the value of the query.
        replace_item(siv, query, timezone_clone, status_text);
    } else {
        // pressing "Enter" without moving the focus into the `matches` view will submit the first match result
        let item = &*matches.selection().unwrap();
        replace_item(siv, item, timezone_clone, status_text);
    };
}

fn seatch_select_view(
    list: Vec<String>,
    status_text: Arc<TextContent>,
    result: Rc<RefCell<String>>,
    name: &str,
) -> Dialog {
    let list_clone = list.clone();
    let locale_clone = result.clone();
    let status_text_clone = status_text.clone();
    let on_edit = move |siv: &mut Cursive, query: &str, _cursor: usize| {
        let matches = search_fn(list.clone(), query);
        // Update the `matches` view with the filtered array of cities
        siv.call_on_name("matches", |v: &mut SelectView| {
            v.clear();
            v.add_all_str(matches);
        });
    };

    wrap_in_dialog(
        LinearLayout::vertical()
            .child(TextView::new(format!("Search {name}")))
            .child(
                EditView::new()
                    // update results every time the query changes
                    .on_edit(on_edit)
                    // submit the focused (first) item of the matches
                    .on_submit(move |s: &mut Cursive, c| {
                        on_submit(s, c, locale_clone.clone(), status_text.clone())
                    })
                    .with_name("query"),
            )
            .child(DummyView {})
            .child(
                SelectView::new()
                    .with_all_str(list_clone)
                    .on_submit(move |s: &mut Cursive, item| {
                        replace_item(s, item, result.clone(), status_text_clone.clone())
                    })
                    .with_name("matches")
                    .scrollable(),
            )
            .fixed_height(10),
        format!("Select Your {name}"),
        None,
    )
}

fn set_timezone(
    zoneinfo: Vec<String>,
    timezone_result: Rc<RefCell<String>>,
    status_text: Arc<TextContent>,
) -> Dialog {
    seatch_select_view(zoneinfo, status_text, timezone_result, "timezone")
}

fn set_locales(
    locales: Vec<String>,
    locale_result: Rc<RefCell<String>>,
    status_text: Arc<TextContent>,
) -> Dialog {
    seatch_select_view(locales, status_text, locale_result, "locale")
}

fn select_swap(siv: &mut Cursive, config: InstallConfig) {
    let config_clone = config.clone();
    let config_clone_2 = config.clone();
    let partition_size = config.partition.as_ref().unwrap().size;
    let installed_size = config.variant.as_ref().unwrap().install_size;
    siv.pop_layer();
    let swap_size = Rc::new(RefCell::new(None));
    let swap_size_copy = Rc::clone(&swap_size);
    let is_hibernation = Arc::new(AtomicBool::new(false));
    let is_hibernation_clone_2 = is_hibernation;
    let use_swap = Arc::new(AtomicBool::new(false));
    let use_swap_clone = use_swap.clone();

    let view = ListView::new().child(
        "Swapfile Size",
        SelectView::new()
            .popup()
            .autojump()
            .with_all_str(vec!["Automatic", "Custom", "Disabled"])
            .with_name("select_swap_config"),
    );

    let textview = TextView::new("Would you like to create a swapfile?\n");
    siv.add_layer(
        wrap_in_dialog(
            LinearLayout::vertical().child(textview).child(view),
            "AOSC OS Installer",
            None,
        )
        .button("Continue", move |s| {
            let selected = s
                .find_name::<SelectView>("select_swap_config")
                .expect("select_swap_config must have value")
                .selected_id()
                .expect("select_swap_config must have value");
            match selected {
                0 => auto_swap(
                    installed_size,
                    partition_size,
                    s,
                    swap_size_copy.clone(),
                    use_swap_clone.clone(),
                    config_clone.clone(),
                ),
                1 => custom_swap_size(
                    installed_size,
                    partition_size,
                    s,
                    swap_size_copy.clone(),
                    config.clone(),
                    is_hibernation_clone_2.clone(),
                    use_swap.clone(),
                ),
                2 => disable_swap(config.clone(), s),
                _ => unreachable!(),
            }
        })
        .button("Back", move |s| {
            s.pop_layer();
            select_timezone(s, config_clone_2.clone());
        })
        .button("Exit", move |s| s.quit()),
    );
}

fn auto_swap(
    installed_size: u64,
    partition_size: u64,
    s: &mut Cursive,
    swap_size: Rc<RefCell<Option<f64>>>,
    use_swap: Arc<AtomicBool>,
    config: InstallConfig,
) {
    let mut config = config;
    let auto_size = disks::get_recommand_swap_size();

    match auto_size {
        Ok(auto_size) => {
            if installed_size + auto_size as u64 > partition_size - DEFAULT_EMPTY_SIZE {
                show_msg(s, &format!("There is not enough available space in the system partition to create a swapfile! Default swapfile size: {} GiB", (auto_size / 1024.0 / 1024.0 / 1024.0).round()));
                return;
            }

            swap_size.replace(Some(auto_size));
            use_swap.store(true, Ordering::SeqCst);
        }
        Err(e) => {
            show_msg(s, &e.to_string());
            return;
        }
    }

    let swap_size = swap_size.as_ref().to_owned().into_inner();
    config.swap_size = Arc::new(swap_size);
    config.use_swap = Arc::new(AtomicBoolWrapper {
        v: AtomicBool::new(use_swap.load(Ordering::SeqCst)),
    });
    config.is_hibernation = Arc::new(AtomicBoolWrapper {
        v: AtomicBool::new(true),
    });

    show_summary(s, config);
}

fn custom_swap_size(
    installed_size: u64,
    partition_size: u64,
    s: &mut Cursive,
    swap_size: Rc<RefCell<Option<f64>>>,
    config: InstallConfig,
    is_hibernation_clone_2: Arc<AtomicBool>,
    use_swap: Arc<AtomicBool>,
) {
    let swap_size_input = Rc::new(RefCell::new(String::new()));
    let swap_size_input_clone = swap_size_input.clone();

    let swap_size_clone = swap_size.clone();
    let use_swap_clone = use_swap.clone();

    let is_hibernation_clone_3 = is_hibernation_clone_2.clone();

    s.add_layer(
        wrap_in_dialog(
            LinearLayout::vertical()
                .child(TextView::new(
                    "Please enter your desired swapfile size (GiB): ",
                ))
                .child(
                    EditView::new()
                        .on_edit_mut(move |_, c, _| {
                            swap_size_input_clone.replace(c.to_owned());
                        })
                        .min_width(20)
                        .with_name("size"),
                ),
            "Customize Swapfile Size",
            None,
        )
        .button("OK", move |s| {
            let mut config = config.clone();
            let size = swap_size_input.as_ref().to_owned().into_inner();
            let size = size.parse::<f64>();
            if size.is_err() {
                show_msg(s, "Invalid custom swapfile size!");
                return;
            }

            let is_hibernation_clone = is_hibernation_clone_2.clone();
            let size = size.unwrap() * 1024.0 * 1024.0 * 1024.0;
            if installed_size + size as u64 > partition_size - DEFAULT_EMPTY_SIZE {
                show_msg(s, &format!("There is not enough space available in the system partition to create a custom swapfile! Custom swapfile size: {} GiB",  (size / 1024.0 / 1024.0 / 1024.0).round()));
                return;
            }

            match disks::is_enable_hibernation(size) {
                Ok(is_h) => {
                    is_hibernation_clone.store(is_h, Ordering::SeqCst);
                    swap_size_clone.replace(Some(size));
                    use_swap_clone.store(true, Ordering::SeqCst);
                }
                Err(e) => {
                    show_msg(s, &e.to_string());
                    return;
                }
            };

            let swap_size = swap_size.as_ref().to_owned().into_inner();
            config.swap_size = Arc::new(swap_size);
            config.use_swap = Arc::new(AtomicBoolWrapper { v: AtomicBool::new(use_swap.load(Ordering::SeqCst) )});
            config.is_hibernation = Arc::new(AtomicBoolWrapper { v: AtomicBool::new(is_hibernation_clone_3.load(Ordering::SeqCst) )});

            show_summary(s, config);
        })
        .button("Cancel", move |s| s.cb_sink().send(Box::new(|s| {
            s.pop_layer();
        }))
        .unwrap()),
    );
}

fn disable_swap(config: InstallConfig, s: &mut Cursive) {
    let mut config = config;
    config.swap_size = Arc::new(None);
    config.use_swap = Arc::new(AtomicBoolWrapper {
        v: AtomicBool::new(false),
    });
    config.is_hibernation = Arc::new(AtomicBoolWrapper {
        v: AtomicBool::new(false),
    });

    show_summary(s, config);
}

fn is_use_last_config(siv: &mut Cursive, config: InstallConfig) {
    siv.pop_layer();
    let config_copy = config.clone();
    siv.add_layer(
        wrap_in_dialog(
            TextView::new(
                "Would you like to load your previous AOSC OS installation configuration?",
            ),
            "AOSC OS Installer",
            None,
        )
        .button("Yes", move |s| show_summary(s, config_copy.clone()))
        .button("No", move |s| {
            fs::remove_file(LAST_USER_CONFIG_FILE).ok();
            let new_config = InstallConfig {
                partition: config.clone().partition,
                ..Default::default()
            };
            select_variant(s, new_config);
        })
        .button("Exit", |s| s.quit()),
    );
}

fn show_summary(siv: &mut Cursive, config: InstallConfig) {
    let mut path = String::new();
    let mut fs = String::new();
    let config_copy = config.clone();
    let config_copy_2 = config.clone();
    if let Some(partition) = config.partition {
        if let Some(partition) = &partition.path {
            path = partition.to_string_lossy().to_string();
        }
        if let Some(fs_type) = &partition.fs_type {
            fs = fs_type.clone();
        }
    }
    let swap_size = if let Some(swap_size) = *config.swap_size {
        swap_size
    } else {
        0.0
    };
    let swap_str;
    match disks::get_recommand_swap_size() {
        Ok(rs) => {
            if swap_size == rs {
                swap_str = "installer default"
            } else if swap_size == 0.0 {
                swap_str = "No swapfile will be created."
            } else {
                swap_str = "custom size"
            };
        }
        Err(e) => {
            show_error(siv, &e.to_string());
            return;
        }
    };
    let s = format!(
        SUMMARY_TEXT!(),
        path,
        fs,
        config.variant.unwrap().name,
        config.mirror.unwrap().name,
        config.user.unwrap(),
        config.locale.unwrap(),
        config.timezone.unwrap(),
        config.tc.unwrap(),
    );
    let swap_s = if swap_size != 0.0 {
        format!(
            "- A {}GiB swapfile will be created and enabled ({}).",
            (swap_size / 1024.0 / 1024.0 / 1024.0).round(),
            swap_str
        )
    } else {
        format!("- {swap_str}")
    };
    siv.add_layer(
        wrap_in_dialog(
            TextView::new(format!("{s}{swap_s}")),
            "Pre-Installation Confirmation",
            None,
        )
        .button("Proceed", move |s| {
            s.pop_layer();
            start_install(s, config_copy.clone());
        })
        .button("Save Configuration", move |s| {
            if let Err(e) = save_user_config_to_file(config_copy_2.clone(), SAVE_USER_CONFIG_FILE) {
                show_error(s, &e.to_string())
            } else {
                show_msg(
                    s,
                    &format!(
                        "Installer has successfully saved your installation configuration: {SAVE_USER_CONFIG_FILE}."
                    ),
                )
            }
        })
        .button("Cancel", |s| {
            s.pop_layer();
        }),
    );
}

fn start_install(siv: &mut Cursive, config: InstallConfig) {
    let logfile = setup_logger(false).expect("Installer could not set logger");
    let logfile_clone = logfile.clone();

    siv.clear_global_callbacks(Event::Exit);
    siv.clear_global_callbacks(Event::CtrlChar('c'));
    add_main_callback(siv);
    ctrlc::set_handler(|| {}).expect("Installer could not initialize SIGINT handler.\n\nPlease restart your installation environment.");
    save_user_config_to_file(config.clone(), LAST_USER_CONFIG_FILE).ok();
    siv.pop_layer();
    let counter = Counter::new(0);
    let counter_clone = counter.clone();
    let mut status_message = TextView::new("");
    let status_text = Arc::new(status_message.get_shared_content());
    let (user_interrup_tx, user_interrup_rx) = std::sync::mpsc::channel();
    siv.add_layer(wrap_in_dialog(
        LinearLayout::vertical()
            .child(TextView::new(
                "Please wait while the installation takes place. This may take minutes or in extreme cases, hours, depending on your device performance.\n\nGot some time to kill? Press <g> to start a game.",
            ))
            .child(DummyView {})
            .child(ProgressBar::new().max(100).with_value(counter))
            .child(status_message),
        "Installing",
        None,
    ).button("Cancel", move |s| {
        let user_interrup_tx = user_interrup_tx.clone();
        s.add_layer(wrap_in_dialog(TextView::new(
            "Installer has not yet completed the installation process. Are you sure that you would like to abort the installation?"), "AOSC OS Installer", None)
            .button("Yes", move |_| {
                user_interrup_tx.send(true).unwrap();
            })
            .button("No", |s| s.cb_sink().send(Box::new(|s| {
                s.pop_layer();
            }))
            .unwrap()));
    }));
    let (tx, rx) = std::sync::mpsc::channel();
    siv.set_autorefresh(true);
    let cb_sink = siv.cb_sink().clone();
    let cb_sink_clone = siv.cb_sink().clone();

    let tempdir = tempfile::Builder::new()
        .prefix(".dkmount")
        .tempdir()
        .expect("Installer failed to create temporary file for the download process.")
        .into_path();

    let tempdir_copy = tempdir.clone();
    let tempdir_copy_2 = tempdir.clone();

    let root_fd = install::get_dir_fd(PathBuf::from("/")).map(|x| x.as_raw_fd())
        .expect("Installer failed to get root file descriptor.\n\nPlease restart your installation environment.");
    let install_thread = thread::spawn(move || begin_install(tx, config, tempdir_copy, logfile));
    thread::spawn(move || {
        let user_exit = user_interrup_rx.recv();
        if let Ok(user_exit) = user_exit {
            if user_exit {
                info!("User request to exit the installer");
                umount_all(&tempdir_copy_2, root_fd);
                cb_sink_clone.send(Box::new(|s| s.quit())).unwrap();
            }
        }
    });
    thread::spawn(move || loop {
        if let Ok(progress) = rx.recv() {
            match progress {
                super::InstallProgress::Pending(msg, pct) => {
                    counter_clone.set(pct);
                    status_text.set_content(&msg);
                }
                super::InstallProgress::Finished => {
                    cb_sink.send(Box::new(show_finished)).unwrap();
                    info!("Install finished");
                    return;
                }
            }
        } else {
            let err = install_thread.join().unwrap().unwrap_err();
            error!("{}", err);

            umount_all(&tempdir, root_fd);
            cb_sink
                .send(Box::new(move |s| {
                    show_error(
                        s,
                        &format!(
                            "{}\n\nPress <~> to see installer log.\n\nLog file is save to {}",
                            err,
                            logfile_clone.display()
                        ),
                    );
                }))
                .unwrap();
            return;
        }
    });
}

fn save_user_config_to_file(config: InstallConfig, path: &str) -> Result<()> {
    let mut config_copy = config;
    config_copy.partition = None;
    let file_str = serde_json::to_string(&config_copy)?;
    fs::File::create(LAST_USER_CONFIG_FILE)?;
    fs::write(path, file_str)?;

    Ok(())
}

fn read_user_config_on_file() -> Result<InstallConfig> {
    let mut file = fs::File::open(LAST_USER_CONFIG_FILE)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    Ok(serde_json::from_slice(&buffer)?)
}

fn show_finished(siv: &mut Cursive) {
    siv.pop_layer();
    siv.add_layer(
        wrap_in_dialog(TextView::new(FINISHED_TEXT), "Installation Complete", None)
            .button("Reboot", |s| {
                install::sync_and_reboot().ok();
                s.quit();
            })
            .button("Exit to LiveKit", |s| s.quit()),
    );
}

pub fn tui_main() {
    let mut siv = cursive::default();

    siv.add_global_callback('~', cursive::Cursive::toggle_debug_console);

    siv.add_layer(
        Dialog::around(TextView::new(WELCOME_TEXT))
            .title("Welcome")
            .button("Let's Go", |s| {
                if let Ok(config) = read_user_config_on_file() {
                    select_partition(s, config);
                } else {
                    let config = InstallConfig::default();
                    select_variant(s, config);
                }
            })
            .padding_lrtb(2, 2, 1, 1)
            .max_width(80),
    );

    siv.run();

    loop {
        let dump = siv.take_user_data::<cursive::Dump>();
        if let Some(dump) = dump {
            drop(siv);
            println!("You may use tools like cfdisk or gdisk to modify your partitions.\nExit the shell (command prompt) to return to the installer.");
            std::process::Command::new("bash")
                .spawn()
                .unwrap()
                .wait()
                .unwrap();
            siv = cursive::default();
            siv.restore(dump);
            let config = siv.take_user_data::<InstallConfig>();
            if let Some(config) = config {
                select_partition(&mut siv, config);
                siv.run();
            }
        } else {
            break;
        }
    }
}
