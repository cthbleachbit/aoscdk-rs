mod disks;
mod install;
mod network;

use cursive::traits::*;
use cursive::utils::{Counter, ProgressReader};
use cursive::view::SizeConstraint;
use cursive::views::{
    Dialog, DummyView, EditView, LinearLayout, ListView, NamedView, Panel, ProgressBar, RadioGroup,
    ResizedView, ScrollView, TextView,
};
use cursive::Cursive;
use number_prefix::NumberPrefix;
use std::convert::TryInto;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::{
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone)]
struct InstallConfig {
    variant: Option<Rc<network::VariantEntry>>,
    partition: Option<Rc<disks::Partition>>,
    mirror: Option<Rc<network::MirrorData>>,
    user: Option<Rc<String>>,
    password: Option<Rc<String>>,
}

fn show_error(siv: &mut Cursive, msg: &str) {
    siv.add_layer(
        Dialog::around(TextView::new(msg))
            .title("Error")
            .button("Exit", |s| s.quit())
            .padding_lrtb(2, 2, 1, 1),
    );
}

fn show_msg(siv: &mut Cursive, msg: &str) {
    siv.add_layer(
        Dialog::around(TextView::new(msg))
            .title("Message")
            .button("OK", |s| {
                s.pop_layer();
            })
            .padding_lrtb(2, 2, 1, 1),
    );
}

fn show_blocking_message(siv: &mut Cursive, msg: &str) {
    siv.add_layer(
        Dialog::around(TextView::new(msg))
            .title("Message")
            .padding_lrtb(2, 2, 1, 1),
    );
}

fn partition_button() -> (&'static str, &'static dyn Fn(&mut Cursive, InstallConfig)) {
    if env::var("DISPLAY").is_ok() {
        return ("Open GParted", &|s, _| {
            show_blocking_message(s, "Waiting for GParted Partitioning Program to finish");
            s.refresh();
            Command::new("gparted").output().ok();
            let new_parts = disks::list_partitions();
            let (disk_list, disk_view) = make_partition_list(new_parts);
            s.set_user_data(disk_list);
            s.call_on_name("part_list", |view: &mut NamedView<LinearLayout>| {
                *view = disk_view;
            });
            s.pop_layer();
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
    let result = match NumberPrefix::binary(size as f64) {
        NumberPrefix::Standalone(bytes) => format!("{} B", bytes),
        NumberPrefix::Prefixed(prefix, n) => format!("{:.1} {}B", n, prefix),
    };

    result
}

fn make_partition_list(
    partitions: Vec<disks::Partition>,
) -> (RadioGroup<disks::Partition>, NamedView<LinearLayout>) {
    let mut disk_view = LinearLayout::vertical();
    let mut disk_list = RadioGroup::new();
    for part in &partitions {
        let path_name;
        if let Some(path) = &part.path {
            path_name = path.to_string_lossy().to_string();
        } else {
            path_name = "?".to_owned();
        }
        let radio = disk_list.button(
            part.clone(),
            format!(
                "{} ({}, {})",
                path_name,
                part.fs_type.as_ref().unwrap_or(&"?".to_owned()),
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
        disk_view.add_child(disk_list.button(dummy_partition, "No partition selected"));
    }

    (disk_list, disk_view.with_name("part_list"))
}

fn select_variant(siv: &mut Cursive, config: InstallConfig) {
    // =cut
    siv.pop_layer();
    siv.add_layer(
        Dialog::around(TextView::new(
            "Downloading distribution information...\nThis can take a while...",
        ))
        .title("Progress"),
    );
    siv.refresh();
    let recipe = network::fetch_recipe();
    if recipe.is_err() {
        show_error(siv, "Could not download recipe information");
        return;
    }
    let variants = recipe.unwrap();
    siv.pop_layer();
    // =cut
    let mut config_view = LinearLayout::vertical();

    let mut variant_list = RadioGroup::new();
    let mut variant_view = LinearLayout::vertical()
        .child(TextView::new(
            "AOSC OS comes in a slew of flavors. From your modern Plasma Desktop and GNOME, timelessly designed MATE Desktop, to your non-graphical Base systems, there is surely one that suits your taste.",
        ))
        .child(DummyView {});
    for variant in variants {
        let radio = variant_list.button(
            variant.clone(),
            format!(
                "{} (Released {}, Download size {})",
                variant.name,
                variant.date,
                human_size(variant.size)
            ),
        );
        variant_view.add_child(radio);
    }
    let variant_view = Panel::new(variant_view).title("Variant");
    config_view.add_child(variant_view);
    config_view.add_child(DummyView {});
    siv.add_layer(
        Dialog::around(ResizedView::new(
            SizeConstraint::AtMost(64),
            SizeConstraint::Free,
            ScrollView::new(config_view),
        ))
        .button("Continue", move |s| {
            let mut config = config.clone();
            config.variant = Some(variant_list.selection());
            select_mirrors(s, config);
        })
        .padding_lrtb(2, 2, 1, 1)
        .title("AOSC OS Installation"),
    );
}

fn select_mirrors(siv: &mut Cursive, config: InstallConfig) {
    // =cut
    siv.pop_layer();
    siv.add_layer(
        Dialog::around(TextView::new(
            "Downloading mirrors information...\nThis can take a while...",
        ))
        .title("Progress"),
    );
    siv.refresh();
    let mirrors = network::fetch_mirrors();
    if mirrors.is_err() {
        show_error(siv, "Could not download mirrors information");
        return;
    }
    let mirrors = mirrors.unwrap();
    siv.pop_layer();
    // =cut
    let mut config_view = LinearLayout::vertical();

    let mut repo_list = RadioGroup::new();
    let mirror_list = mirrors.mirrors;
    let mut repo_view = LinearLayout::vertical()
        .child(TextView::new(
            "Please select a mirror from which you would like to download AOSC OS and the extra components you specified. Generally, a mirror closest to you geographically would be the best bet for download speeds.",
        ))
        .child(DummyView {});
    for mirror in mirror_list {
        let radio = repo_list.button(
            mirror.clone(),
            format!("{} ({})", mirror.name, mirror.region),
        );
        repo_view.add_child(radio);
    }
    let repo_view = Panel::new(repo_view).title("Repositories");
    config_view.add_child(repo_view);
    config_view.add_child(DummyView {});
    siv.add_layer(
        Dialog::around(ResizedView::new(
            SizeConstraint::AtMost(64),
            SizeConstraint::Free,
            ScrollView::new(config_view),
        ))
        .button("Continue", move |s| {
            let mut config = config.clone();
            config.mirror = Some(repo_list.selection());
            select_partition(s, config);
        })
        .padding_lrtb(2, 2, 1, 1)
        .title("AOSC OS Installation"),
    );
}

fn select_partition(siv: &mut Cursive, config: InstallConfig) {
    // =cut
    siv.pop_layer();
    siv.add_layer(
        Dialog::around(TextView::new("Probing disks...\nThis can take a while..."))
            .title("Progress"),
    );
    siv.refresh();
    let partitions = disks::list_partitions();
    siv.pop_layer();
    // =cut
    let mut config_view = LinearLayout::vertical();
    let (disk_list, disk_view) = make_partition_list(partitions);
    siv.set_user_data(disk_list);
    let dest_view = LinearLayout::vertical()
        .child(TextView::new(
            "Please select a partition to which you would like to install AOSC OS onto. If you would like to make changes to your partitions, please click on \"Open GParted.\"",
        ))
        .child(DummyView {})
        .child(disk_view);
    let dest_view = Panel::new(dest_view).title("Destination");
    config_view.add_child(dest_view);
    config_view.add_child(DummyView {});
    let (btn_label, btn_cb) = partition_button();
    let config_copy = config.clone();
    siv.add_layer(
        Dialog::around(ResizedView::new(
            SizeConstraint::AtMost(64),
            SizeConstraint::Free,
            ScrollView::new(config_view),
        ))
        .button(btn_label, move |s| {
            btn_cb(s, config_copy.clone());
        })
        .button("Continue", move |s| {
            let disk_list = s.user_data::<RadioGroup<disks::Partition>>();
            if let Some(disk_list) = disk_list {
                let disk_list = disk_list.clone();
                let current_partition;
                if cfg!(debug_assertions) {
                    // prevent developer/tester accidentally delete their partitions
                    current_partition = Rc::new(disks::Partition {
                        fs_type: None,
                        path: Some(PathBuf::from("/dev/loop0p1")),
                        parent_path: Some(PathBuf::from("/dev/loop0")),
                        size: 3145728,
                    });
                } else {
                    current_partition = disk_list.selection();
                }
                if current_partition.parent_path.is_none() && current_partition.size == 0 {
                    show_msg(s, "Please specify a partition.");
                    s.refresh();
                    return;
                }
                let mut config = config.clone();
                let new_part = disks::fill_fs_type(current_partition.as_ref());
                config.partition = Some(Rc::new(new_part));
                show_summary(s, config);
            }
        })
        .padding_lrtb(2, 2, 1, 1)
        .title("AOSC OS Installation"),
    );
}

fn select_user(siv: &mut Cursive, config: &InstallConfig) {
    siv.pop_layer();
    let config_view = ListView::new()
        .child("Username", EditView::new().min_width(20))
        .child("Password", EditView::new().min_width(20))
        .child("Confirm Password", EditView::new().min_width(20));
    siv.add_layer(
        Dialog::around(ResizedView::new(
            SizeConstraint::AtMost(64),
            SizeConstraint::Free,
            ScrollView::new(config_view),
        ))
        .padding_lrtb(2, 2, 1, 1)
        .title("AOSC OS Installation")
        .button("Continue", |s| {
            // TODO:
        }),
    );
}

fn show_summary(siv: &mut Cursive, config: InstallConfig) {
    let mut path = String::new();
    let mut fs = String::new();
    let config_copy = config.clone();
    if let Some(partition) = config.partition {
        if let Some(partition) = &partition.path {
            path = partition.to_string_lossy().to_string();
        }
        if let Some(fs_type) = &partition.fs_type {
            fs = fs_type.clone();
        }
    }
    siv.add_layer(
        Dialog::around(ResizedView::new(
            SizeConstraint::AtMost(64),
            SizeConstraint::Free,
            ScrollView::new(
                TextView::new(format!("The following actions will be performed:\n- {} will be erased and formatted as {}.\n- AOSC OS {} variant will be installed using {} mirror server.",
                path, fs, config.variant.unwrap().name, config.mirror.unwrap().name))
            ),
        ))
        .title("Confirmation")
        .button("Cancel", |s| {
            s.pop_layer();
        })
        .button("Install", move |s| {
            s.pop_layer();
            begin_install(s, config_copy.clone());
        })
    );
}

fn begin_install(siv: &mut Cursive, config: InstallConfig) {
    siv.pop_layer();
    let refresh_interval = std::time::Duration::from_millis(300);
    let status_text = TextView::new("").with_name("status");
    let counter = Counter::new(0);
    let url;
    let file_size: usize;
    let download_done: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let extract_done: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    siv.call_on_name("status", |v: &mut NamedView<TextView>| {
        v.get_mut()
            .set_content("Step 1 of 5: Formatting partition...");
    });
    siv.refresh();
    let partition = &config.partition.unwrap();
    if let Err(e) = disks::format_partition(partition) {
        show_error(siv, &e.to_string());
        return;
    }
    let mount_path = install::auto_mount_root_path(partition);
    if let Err(e) = mount_path {
        show_error(siv, &e.to_string());
        return;
    }
    let mount_path = mount_path.unwrap();
    let mount_path_copy = mount_path.clone();
    let mount_path_copy2 = mount_path.clone();
    if disks::is_efi_booted() {
        let mut efi_path = mount_path.clone();
        efi_path.push("efi");
        let esp_part = disks::find_esp_partition(partition.parent_path.as_ref().unwrap());
        if let Err(e) = esp_part {
            show_error(siv, &e.to_string());
            return;
        }
        let esp_part = esp_part.unwrap();
        std::fs::create_dir_all(&efi_path).unwrap();
        if let Err(e) = install::mount_root_path(&esp_part, &efi_path) {
            show_error(siv, &e.to_string());
            return;
        }
    }
    if let Some(variant) = config.variant.as_ref() {
        file_size = variant.size.try_into().unwrap();
        url = variant.url.clone();
    } else {
        return;
    }
    let download_done_copy = download_done.clone();
    let extract_done_copy = extract_done.clone();
    let progress_bar = ProgressBar::new()
        .max(file_size)
        .with_value(counter.clone())
        .with_task(move |counter| {
            let mut tarball_file = mount_path.clone();
            tarball_file.push("tarball");
            let mut output;
            if let Ok(reader) = network::download_file(&url) {
                let mut reader = ProgressReader::new(counter.clone(), reader);
                output = std::fs::File::create(tarball_file.clone()).unwrap();
                std::io::copy(&mut reader, &mut output).unwrap();
                download_done_copy.fetch_or(true, Ordering::SeqCst);
            } else {
                return;
            }
            counter.clone().set(0);
            output = std::fs::File::open(tarball_file.clone()).unwrap();
            let reader = ProgressReader::new(counter.clone(), output);
            install::extract_tar_xz(reader, &mount_path_copy).unwrap();
            extract_done_copy.fetch_or(true, Ordering::SeqCst);
            std::fs::remove_file(tarball_file).ok();
        });
    siv.add_layer(
        Dialog::around(
            LinearLayout::vertical().child(
                TextView::new("Please wait while the installation is taking place.\nDuring installation, you may want to go around and get a feeling for AOSC OS!")
            ).child(DummyView {}).child(progress_bar).child(status_text)
        ).title("Installing")
    );
    siv.call_on_name("status", |v: &mut NamedView<TextView>| {
        v.get_mut()
            .set_content("Step 2 of 5: Downloading tarball...");
    });
    loop {
        if download_done.load(Ordering::SeqCst) {
            break;
        }
        siv.refresh();
        std::thread::sleep(refresh_interval);
    }
    siv.call_on_name("status", |v: &mut NamedView<TextView>| {
        v.get_mut()
            .set_content("Step 3 of 5: Extracting tarball...");
    });
    loop {
        if extract_done.load(Ordering::SeqCst) {
            break;
        }
        siv.refresh();
        std::thread::sleep(refresh_interval);
    }
    siv.refresh();
    siv.call_on_name("status", |v: &mut NamedView<TextView>| {
        v.get_mut()
            .set_content("Step 4 of 5: Generating initial RAM disk...");
    });
    let distance = install::get_root_distance(&mount_path_copy2);
    install::remove_bind_mounts(&mount_path_copy2).ok();
    install::dive_into_guest(&mount_path_copy2).unwrap();
    install::execute_dracut().unwrap();
    if let Err(e) = distance {
        show_error(siv, &e.to_string());
        return;
    }
    siv.refresh();
    siv.call_on_name("status", |v: &mut NamedView<TextView>| {
        v.get_mut()
            .set_content("Step 5 of 5: Writing GRUB bootloader...");
    });
    if disks::is_efi_booted() {
        install::execute_grub_install(None).unwrap();
    } else {
        install::execute_grub_install(Some(partition.parent_path.as_ref().unwrap())).unwrap();
    }
    install::escape_chroot(distance.unwrap()).unwrap();
    install::remove_bind_mounts(&mount_path_copy2).ok();
}

fn main() {
    let mut siv = cursive::default();
    siv.add_layer(
        Dialog::around(TextView::new("Welcome to AOSC OS installer!"))
            .title("Welcome")
            .button("Start", |s| {
                let config = InstallConfig {
                    variant: None,
                    partition: None,
                    mirror: None,
                    user: None,
                    password: None,
                };
                select_variant(s, config)
            })
            .padding_lrtb(2, 2, 1, 1),
    );
    siv.run();
    loop {
        let dump = siv.take_user_data::<cursive::Dump>();
        if let Some(dump) = dump {
            drop(siv);
            println!("You can use tools like cfdisk or gdisk to modify your partitions.\nExit the shell to return to the installer.");
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
