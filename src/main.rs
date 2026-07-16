mod telemetry;

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::{gio, glib, Align, Orientation};
use telemetry::{ComponentInfo, DeviceInfo, DiskUsage, SensorReading, Snapshot, TelemetryCollector};

const APP_ID: &str = "io.github.mars7x.hardware-info-gtk4";
const HISTORY_LENGTH: usize = 180;

const APP_CSS: &str = r#"
.metric-card {
  border-radius: 12px;
}

flowbox.metric-flow > flowboxchild {
  border-radius: 12px;
  padding: 0;
}

/* Overview cards are information-only. Keep pointer hover visually inert while
 * leaving GTK's native keyboard focus state untouched. */
flowbox.metric-flow > flowboxchild:hover:not(:focus),
flowbox.metric-flow > flowboxchild:active:not(:focus) {
  background-color: @card_bg_color;
  background-image: none;
}

.cpu-amd-accent { color: #ff7800; }
.cpu-intel-accent { color: #3584e4; }
.memory-accent { color: #9141ac; }
.gpu-amd-accent { color: #e01b24; }
.gpu-nvidia-accent { color: #33d17a; }
.gpu-intel-accent { color: #3584e4; }
.gpu-neutral-accent { color: #77767b; }
.storage-accent { color: #2190a4; }
.battery-accent { color: #e5a50a; }
.neutral-accent { color: @accent_color; }

progressbar.cpu-amd-accent progress { background-color: #ff7800; }
progressbar.cpu-intel-accent progress { background-color: #3584e4; }
progressbar.memory-accent progress { background-color: #9141ac; }
progressbar.gpu-amd-accent progress { background-color: #e01b24; }
progressbar.gpu-nvidia-accent progress { background-color: #33d17a; }
progressbar.gpu-intel-accent progress { background-color: #3584e4; }
progressbar.gpu-neutral-accent progress { background-color: #77767b; }
progressbar.storage-accent progress { background-color: #2190a4; }
progressbar.battery-accent progress { background-color: #e5a50a; }

progressbar.device-battery-high progress { background-color: #2ec27e; }
progressbar.device-battery-medium progress { background-color: #f5c211; }
progressbar.device-battery-low progress { background-color: #e01b24; }

"#;

const ACCENT_CLASSES: [&str; 10] = [
    "cpu-amd",
    "cpu-intel",
    "memory",
    "gpu-amd",
    "gpu-nvidia",
    "gpu-intel",
    "gpu-neutral",
    "storage",
    "battery",
    "neutral",
];

#[derive(Clone)]
struct HistoryGraph {
    area: gtk::DrawingArea,
    values: Rc<RefCell<VecDeque<f64>>>,
}

impl HistoryGraph {
    fn new(max_value: f64) -> Self {
        let values = Rc::new(RefCell::new(VecDeque::<f64>::with_capacity(HISTORY_LENGTH)));
        let draw_values = values.clone();

        let area = gtk::DrawingArea::builder()
            .content_height(82)
            .hexpand(true)
            .focusable(false)
            .can_target(false)
            .build();

        area.set_draw_func(move |area, context, width, height| {
            let values = draw_values.borrow();
            if values.len() < 2 || width <= 8 || height <= 8 {
                return;
            }

            let color = area.color();
            let red = color.red() as f64;
            let green = color.green() as f64;
            let blue = color.blue() as f64;
            let padding = 4.0;
            let graph_width = width as f64 - padding * 2.0;
            let graph_height = height as f64 - padding * 2.0;

            context.set_line_width(1.0);
            context.set_source_rgba(red, green, blue, 0.12);
            for step in 1..4 {
                let y = padding + graph_height * step as f64 / 4.0;
                context.move_to(padding, y);
                context.line_to(width as f64 - padding, y);
            }
            let _ = context.stroke();

            let scale_max = if max_value > 0.0 {
                max_value
            } else {
                values
                    .iter()
                    .copied()
                    .fold(1.0_f64, f64::max)
                    .mul_add(1.10, 0.0)
            };
            let points = values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let x = padding
                        + graph_width * index as f64 / (values.len().saturating_sub(1)) as f64;
                    let normalized = (value / scale_max).clamp(0.0, 1.0);
                    let y = padding + graph_height * (1.0 - normalized);
                    (x, y)
                })
                .collect::<Vec<_>>();

            context.move_to(points[0].0, height as f64 - padding);
            for (x, y) in &points {
                context.line_to(*x, *y);
            }
            context.line_to(points[points.len() - 1].0, height as f64 - padding);
            context.close_path();
            context.set_source_rgba(red, green, blue, 0.10);
            let _ = context.fill();

            context.move_to(points[0].0, points[0].1);
            for (x, y) in points.iter().skip(1) {
                context.line_to(*x, *y);
            }
            context.set_source_rgba(red, green, blue, 0.92);
            context.set_line_width(2.0);
            let _ = context.stroke();
        });

        Self { area, values }
    }

    fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    fn push(&self, value: Option<f64>) {
        let Some(value) = value.filter(|value| value.is_finite()) else {
            return;
        };

        let mut values = self.values.borrow_mut();
        if values.len() == HISTORY_LENGTH {
            values.pop_front();
        }
        values.push_back(value);
        drop(values);
        self.area.queue_draw();
    }
}

#[derive(Clone)]
struct AnimatedProgress {
    bar: gtk::ProgressBar,
    current: Rc<Cell<f64>>,
    target: Rc<Cell<f64>>,
}

impl AnimatedProgress {
    fn new() -> Self {
        let bar = gtk::ProgressBar::builder()
            .hexpand(true)
            .show_text(false)
            .build();
        let current: Rc<Cell<f64>> = Rc::new(Cell::new(0.0_f64));
        let target: Rc<Cell<f64>> = Rc::new(Cell::new(0.0_f64));
        let current_for_tick = current.clone();
        let target_for_tick = target.clone();

        bar.add_tick_callback(move |bar, _| {
            let current = current_for_tick.get();
            let target = target_for_tick.get();
            let difference: f64 = target - current;
            let next: f64 = if difference.abs() < 0.001_f64 {
                target
            } else {
                current + difference * 0.16
            };
            current_for_tick.set(next);
            bar.set_fraction(next.clamp(0.0, 1.0));
            glib::ControlFlow::Continue
        });

        Self {
            bar,
            current,
            target,
        }
    }

    fn widget(&self) -> &gtk::ProgressBar {
        &self.bar
    }

    fn set_fraction(&self, fraction: Option<f64>) {
        self.bar.set_visible(fraction.is_some());
        if let Some(fraction) = fraction {
            // Keep the bar synchronized with the whole percentage shown to the user.
            // Small decimal-only telemetry changes therefore do not move the bar.
            let whole_percent = (fraction.clamp(0.0, 1.0) * 100.0).round();
            self.target.set(whole_percent / 100.0);
        } else {
            self.current.set(0.0);
            self.target.set(0.0);
            self.bar.set_fraction(0.0);
        }
    }
}

#[derive(Clone)]
struct MetricCard {
    card: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    value: gtk::Label,
    subtitle: gtk::Label,
    progress: AnimatedProgress,
    graph: HistoryGraph,
    graph_caption: gtk::Label,
}

impl MetricCard {
    fn new(title: &str, icon_name: &str, graph_max: f64, accent: &str) -> Self {
        let icon = app_icon_image(icon_name, 22);

        let title_label = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .tooltip_text(title)
            .css_classes(["heading"])
            .build();

        let header = gtk::Box::new(Orientation::Horizontal, 8);
        header.append(&icon);
        header.append(&title_label);

        let value = gtk::Label::builder()
            .label("—")
            .xalign(0.0)
            .css_classes(["title-1", "numeric"])
            .build();

        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(48)
            .css_classes(["dim-label"])
            .build();

        let progress = AnimatedProgress::new();

        let graph = HistoryGraph::new(graph_max);
        let graph_caption = gtk::Label::builder()
            .label("Last 3 minutes")
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build();

        let content = gtk::Box::new(Orientation::Vertical, 10);
        content.set_margin_top(18);
        content.set_margin_bottom(16);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.append(&header);
        content.append(&value);
        content.append(&subtitle);
        content.append(progress.widget());
        content.append(graph.widget());
        content.append(&graph_caption);

        let card = gtk::Box::new(Orientation::Vertical, 0);
        card.set_size_request(280, -1);
        // The generated GtkFlowBoxChild owns the visible card surface and focus.
        // This content box remains informational and does not accept pointer input.
        card.set_focusable(false);
        card.set_can_target(false);
        card.append(&content);

        let result = Self {
            card,
            icon,
            title: title_label,
            value,
            subtitle,
            progress,
            graph,
            graph_caption,
        };
        result.set_accent(accent);
        result
    }

    fn set_title(&self, title: &str) {
        let title = if title.trim().is_empty() { "Device" } else { title.trim() };
        self.title.set_label(title);
        self.title.set_tooltip_text(Some(title));
    }

    fn set_graph_visible(&self, visible: bool) {
        self.graph.area.set_visible(visible);
        self.graph_caption.set_visible(visible);
    }

    fn set_accent(&self, accent: &str) {
        for class in ACCENT_CLASSES {
            self.icon.remove_css_class(&format!("{class}-accent"));
            self.value.remove_css_class(&format!("{class}-accent"));
            self.progress.bar.remove_css_class(&format!("{class}-accent"));
            self.graph.area.remove_css_class(&format!("{class}-accent"));
        }
        self.icon.add_css_class(&format!("{accent}-accent"));
        self.value.add_css_class(&format!("{accent}-accent"));
        self.progress.bar.add_css_class(&format!("{accent}-accent"));
        self.graph.area.add_css_class(&format!("{accent}-accent"));
    }

    fn set(&self, value: &str, subtitle: &str, fraction: Option<f64>, history: Option<f64>) {
        self.value.set_label(value);
        self.subtitle.set_label(subtitle);
        self.subtitle.set_visible(!subtitle.trim().is_empty());
        self.progress.set_fraction(fraction);
        self.graph.push(history);
    }
}

#[derive(Clone)]
struct SensorDisplay {
    key: String,
    title: String,
    subtitle: String,
    value: String,
    icon_name: String,
}

#[derive(Clone)]
struct SensorRowUi {
    key: String,
    row: adw::ActionRow,
    value: gtk::Label,
}

#[derive(Clone)]
struct DeviceRowUi {
    key: String,
    row: adw::ActionRow,
    icon: gtk::Image,
    battery_box: gtk::Box,
    battery_label: gtk::Label,
    battery_bar: gtk::ProgressBar,
}

#[derive(Clone)]
struct Ui {
    cpu: MetricCard,
    memory: MetricCard,
    performance_flow: gtk::FlowBox,
    gpu_cards: Rc<RefCell<Vec<(String, MetricCard)>>>,
    disk_flow: gtk::FlowBox,
    disk_section: gtk::Box,
    disk_cards: Rc<RefCell<Vec<(String, MetricCard)>>>,
    battery_flow: gtk::FlowBox,
    battery_section: gtk::Box,
    battery_cards: Rc<RefCell<Vec<(String, MetricCard)>>>,
    devices_list: gtk::ListBox,
    device_rows: Rc<RefCell<Vec<DeviceRowUi>>>,
    devices_stack: gtk::Stack,
    hardware_page: adw::PreferencesPage,
    hardware_stack: gtk::Stack,
    hardware_initialized: Rc<Cell<bool>>,
    temperature_list: gtk::ListBox,
    temperature_rows: Rc<RefCell<Vec<SensorRowUi>>>,
    other_sensors_list: gtk::ListBox,
    other_sensor_rows: Rc<RefCell<Vec<SensorRowUi>>>,
    temperature_section: gtk::Box,
    other_sensors_section: gtk::Box,
    diagnostics_section: gtk::Box,
    spinner: gtk::Spinner,
}

impl Ui {
    fn update(&self, snapshot: &Snapshot) {
        let cpu_name = concise_cpu_name(&snapshot.static_info.cpu_model);
        self.cpu.set_title(if is_meaningful_text(&cpu_name) {
            &cpu_name
        } else {
            "Processor"
        });
        self.cpu
            .set_accent(cpu_accent(&snapshot.static_info.cpu_model));

        let mut cpu_details = Vec::new();
        if let Some(value) = snapshot.cpu_temp_c {
            cpu_details.push(format!("{value:.0} °C"));
        }
        if let Some(value) = snapshot.cpu_frequency_mhz {
            cpu_details.push(format_frequency(value));
        }
        self.cpu.set(
            &format!("{:.0}%", snapshot.cpu_usage_percent),
            &cpu_details.join(" · "),
            Some(snapshot.cpu_usage_percent / 100.0),
            Some(snapshot.cpu_usage_percent),
        );

        let memory_fraction = ratio(snapshot.memory_used_bytes, snapshot.memory_total_bytes);
        let memory_percent = memory_fraction.map(|value| value * 100.0);
        let memory_subtitle = if snapshot.memory_total_bytes > 0 {
            format!(
                "{} of {} used · {} available",
                format_bytes(snapshot.memory_used_bytes),
                format_bytes(snapshot.memory_total_bytes),
                format_bytes(snapshot.memory_available_bytes)
            )
        } else {
            String::new()
        };
        self.memory.set(
            &memory_percent
                .map(|value| format!("{value:.0}%"))
                .unwrap_or_else(|| "—".to_string()),
            &memory_subtitle,
            memory_fraction,
            memory_percent,
        );

        self.update_gpu_cards(snapshot);
        self.update_disk_cards(snapshot);
        self.update_battery_cards(snapshot);

        let device_count = sync_device_rows(
            &self.devices_list,
            &self.device_rows,
            &snapshot.devices,
        );
        self.devices_stack.set_visible_child_name(if device_count > 0 {
            "content"
        } else {
            "empty"
        });

        if !self.hardware_initialized.get() {
            populate_hardware_inventory(&self.hardware_page, snapshot);
            self.hardware_stack.set_visible_child_name("content");
            self.hardware_initialized.set(true);
        }

        let temperature_count = sync_sensor_rows(
            &self.temperature_list,
            &self.temperature_rows,
            temperature_displays(snapshot),
        );
        let other_count = sync_sensor_rows(
            &self.other_sensors_list,
            &self.other_sensor_rows,
            other_sensor_displays(snapshot),
        );
        self.temperature_section.set_visible(temperature_count > 0);
        self.other_sensors_section.set_visible(other_count > 0);
        self.diagnostics_section
            .set_visible(temperature_count + other_count > 0);

        self.spinner.stop();
        self.spinner.set_visible(false);
    }

    fn update_gpu_cards(&self, snapshot: &Snapshot) {
        let keys = snapshot
            .gpus
            .iter()
            .enumerate()
            .map(|(index, gpu)| format!("{index}:{}", gpu.name))
            .collect::<Vec<_>>();
        let old_keys = self
            .gpu_cards
            .borrow()
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        if keys != old_keys {
            // CPU and memory are permanent children. Removing the whole flow can leave
            // those cards detached on some GTK versions, so only replace GPU cards.
            for (_, card) in self.gpu_cards.borrow().iter() {
                if let Some(parent) = card.card.parent() {
                    self.performance_flow.remove(&parent);
                }
            }

            let mut cards = Vec::new();
            for (index, gpu) in snapshot.gpus.iter().enumerate() {
                let title = concise_gpu_name(&gpu.name);
                let card = MetricCard::new(
                    if is_meaningful_text(&title) { &title } else { "Graphics" },
                    themed_icon_name("gpu-symbolic", "video-display-symbolic"),
                    100.0,
                    gpu_accent(&gpu.name),
                );
                // Keep Processor first and Memory last so the first GPU sits beside CPU.
                insert_metric_card(&self.performance_flow, &card.card, (index + 1) as i32);
                cards.push((format!("{index}:{}", gpu.name), card));
            }

            *self.gpu_cards.borrow_mut() = cards;
        }

        for ((_, card), gpu) in self.gpu_cards.borrow().iter().zip(&snapshot.gpus) {
            let title = concise_gpu_name(&gpu.name);
            card.set_title(if is_meaningful_text(&title) {
                &title
            } else {
                "Graphics"
            });
            card.set_accent(gpu_accent(&gpu.name));
            let mut details = Vec::new();
            if let Some(value) = gpu.temperature_c {
                details.push(format!("{value:.0} °C"));
            }
            if let (Some(used), Some(total)) = (gpu.memory_used_bytes, gpu.memory_total_bytes) {
                if total > 0 {
                    details.push(format!("VRAM {} / {}", format_bytes(used), format_bytes(total)));
                }
            }
            if let Some(value) = gpu.power_watts.filter(|value| *value > 0.0) {
                details.push(format!("{value:.1} W"));
            }
            if let Some(value) = gpu.fan_percent.filter(|value| *value > 0.0) {
                details.push(format!("Fan {value:.0}%"));
            }
            card.set(
                &gpu.usage_percent
                    .map(|value| format!("{value:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
                &details.join(" · "),
                gpu.usage_percent.map(|value| value / 100.0),
                gpu.usage_percent,
            );
        }
    }

    fn update_disk_cards(&self, snapshot: &Snapshot) {
        let keys = snapshot
            .disks
            .iter()
            .map(|disk| disk.device.clone())
            .collect::<Vec<_>>();
        let old_keys = self
            .disk_cards
            .borrow()
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        if keys != old_keys {
            clear_flow(&self.disk_flow);
            let mut cards = Vec::new();
            for disk in &snapshot.disks {
                let card = MetricCard::new(
                    &disk_title(disk),
                    "drive-harddisk-symbolic",
                    0.0,
                    "storage",
                );
                card.set_graph_visible(false);
                insert_metric_card(&self.disk_flow, &card.card, -1);
                cards.push((disk.device.clone(), card));
            }
            *self.disk_cards.borrow_mut() = cards;
        }

        self.disk_section.set_visible(!snapshot.disks.is_empty());
        for ((_, card), disk) in self.disk_cards.borrow().iter().zip(&snapshot.disks) {
            card.set_title(&disk_title(disk));
            let fraction = ratio(disk.used_bytes, disk.total_bytes);
            let percent = fraction.map(|value| value * 100.0);
            let mut details = vec![format!(
                "{} of {} used · {} available",
                format_bytes(disk.used_bytes),
                format_bytes(disk.total_bytes),
                format_bytes(disk.available_bytes)
            )];
            if let Some(value) = disk.temperature_c {
                details.push(format!("{value:.0} °C"));
            }
            card.set(
                &percent
                    .map(|value| format!("{value:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
                &details.join(" · "),
                fraction,
                None,
            );
        }
    }

    fn update_battery_cards(&self, snapshot: &Snapshot) {
        let keys = snapshot
            .batteries
            .iter()
            .enumerate()
            .map(|(index, battery)| format!("{index}:{}", battery.name))
            .collect::<Vec<_>>();
        let old_keys = self
            .battery_cards
            .borrow()
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        if keys != old_keys {
            clear_flow(&self.battery_flow);
            let mut cards = Vec::new();
            for (index, battery) in snapshot.batteries.iter().enumerate() {
                let card = MetricCard::new(
                    &battery.name,
                    "battery-symbolic",
                    100.0,
                    "battery",
                );
                insert_metric_card(&self.battery_flow, &card.card, -1);
                cards.push((format!("{index}:{}", battery.name), card));
            }
            *self.battery_cards.borrow_mut() = cards;
        }

        self.battery_section
            .set_visible(!snapshot.batteries.is_empty());
        for ((_, card), battery) in self
            .battery_cards
            .borrow()
            .iter()
            .zip(&snapshot.batteries)
        {
            let mut details = Vec::new();
            if is_meaningful_text(&battery.status) {
                details.push(battery.status.clone());
            }
            if let Some(value) = battery.power_watts.filter(|value| *value > 0.0) {
                details.push(format!("{value:.1} W"));
            }
            if let Some(value) = battery.energy_full_wh.filter(|value| *value > 0.0) {
                details.push(format!("{value:.1} Wh full"));
            }
            let percentage = battery.capacity_percent;
            card.set(
                &percentage
                    .map(|value| format!("{value:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
                &details.join(" · "),
                percentage.map(|value| value / 100.0),
                percentage,
            );
        }
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();

    let quit_action = gio::SimpleAction::new("quit", None);
    let app_for_quit = app.clone();
    quit_action.connect_activate(move |_, _| app_for_quit.quit());
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<primary>q"]);

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    install_css();

    let (snapshot_tx, snapshot_rx) = mpsc::channel::<Snapshot>();
    thread::spawn(move || {
        let mut collector = TelemetryCollector::new();
        loop {
            let snapshot = collector.collect();
            if snapshot_tx.send(snapshot).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    });

    let cpu = MetricCard::new(
        "Processor",
        "processor-symbolic",
        100.0,
        "neutral",
    );
    let memory = MetricCard::new("Memory", "media-flash-symbolic", 100.0, "memory");

    let performance_flow = metric_flow();
    cpu.card.set_visible(true);
    memory.card.set_visible(true);
    insert_metric_card(&performance_flow, &cpu.card, -1);
    insert_metric_card(&performance_flow, &memory.card, -1);

    let disk_flow = metric_flow();
    let battery_flow = metric_flow();

    let performance_section = metric_section("Performance", &performance_flow);
    let disk_section = metric_section("Drive usage", &disk_flow);
    let battery_section = metric_section("Batteries", &battery_flow);
    disk_section.set_visible(false);
    battery_section.set_visible(false);

    let overview_content = gtk::Box::new(Orientation::Vertical, 26);
    overview_content.set_margin_top(24);
    overview_content.set_margin_bottom(32);
    overview_content.set_margin_start(18);
    overview_content.set_margin_end(18);
    overview_content.append(&performance_section);
    overview_content.append(&disk_section);
    overview_content.append(&battery_section);

    let overview_clamp = adw::Clamp::builder()
        .maximum_size(1080)
        .tightening_threshold(720)
        .child(&overview_content)
        .build();
    let overview_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&overview_clamp)
        .build();

    let devices_list = boxed_list();
    let devices_content = gtk::Box::new(Orientation::Vertical, 0);
    devices_content.set_margin_top(24);
    devices_content.set_margin_bottom(32);
    devices_content.set_margin_start(18);
    devices_content.set_margin_end(18);
    devices_content.append(&devices_list);
    let devices_clamp = adw::Clamp::builder()
        .maximum_size(760)
        .tightening_threshold(560)
        .child(&devices_content)
        .build();
    let devices_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&devices_clamp)
        .build();
    let devices_empty = adw::StatusPage::builder()
        .icon_name("applications-games-symbolic")
        .title("No supported devices found")
        .description("Connected keyboards, mice, controllers, and headsets will appear here when Linux exposes them.")
        .build();
    let devices_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(180)
        .build();
    devices_stack.add_named(&devices_scroll, Some("content"));
    devices_stack.add_named(&devices_empty, Some("empty"));
    devices_stack.set_visible_child_name("empty");

    let hardware_page = adw::PreferencesPage::new();
    let hardware_loading = adw::StatusPage::builder()
        .icon_name("computer-symbolic")
        .title("Detecting hardware")
        .description("Reading the computer's static hardware inventory.")
        .build();
    let hardware_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    hardware_stack.add_named(&hardware_loading, Some("loading"));
    hardware_stack.add_named(&hardware_page, Some("content"));
    hardware_stack.set_visible_child_name("loading");

    let temperature_list = boxed_list();
    let other_sensors_list = boxed_list();
    let temperature_section = list_section(
        "Additional temperatures",
        "Memory and motherboard readings not already shown on device cards.",
        &temperature_list,
    );
    let other_sensors_section = list_section(
        "Cooling and power",
        "Available fan speeds and useful system or processor power readings.",
        &other_sensors_list,
    );

    let diagnostics_section = gtk::Box::new(Orientation::Vertical, 20);
    diagnostics_section.append(&temperature_section);
    diagnostics_section.append(&other_sensors_section);
    diagnostics_section.set_visible(false);

    overview_content.append(&diagnostics_section);

    let gpu_cards = Rc::new(RefCell::new(Vec::new()));
    let disk_cards = Rc::new(RefCell::new(Vec::new()));
    let battery_cards = Rc::new(RefCell::new(Vec::new()));
    let device_rows = Rc::new(RefCell::new(Vec::new()));
    let temperature_rows = Rc::new(RefCell::new(Vec::new()));
    let other_sensor_rows = Rc::new(RefCell::new(Vec::new()));

    let content_stack = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(true)
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(180)
        .build();
    content_stack.add_named(&overview_scroll, Some("overview"));
    content_stack.add_named(&devices_stack, Some("devices"));
    content_stack.add_named(&hardware_stack, Some("hardware"));
    content_stack.set_visible_child_name("overview");

    let overview_nav = navigation_row("view-grid-symbolic", "Overview");
    let devices_nav = navigation_row("applications-games-symbolic", "Devices");
    let hardware_nav = navigation_row("computer-symbolic", "Hardware");

    let navigation = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["navigation-sidebar"])
        .build();
    navigation.append(&overview_nav);
    navigation.append(&devices_nav);
    navigation.append(&hardware_nav);
    navigation.select_row(Some(&overview_nav));

    let menu = gio::Menu::new();
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About hardware-info-gtk4"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(&menu)
        .build();

    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.set_show_back_button(false);
    sidebar_header.set_title_widget(Some(
        &adw::WindowTitle::builder().title("hardware-info-gtk4").build(),
    ));
    sidebar_header.pack_end(&menu_button);

    let navigation_box = gtk::Box::new(Orientation::Vertical, 0);
    navigation_box.add_css_class("sidebar-content");
    navigation_box.set_size_request(220, -1);
    navigation_box.append(&navigation);

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(&navigation_box));

    let content_title = adw::WindowTitle::builder().title("Overview").build();
    let sidebar_toggle = gtk::Button::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Hide Sidebar")
        .build();

    let spinner = gtk::Spinner::new();
    spinner.start();

    let content_header = adw::HeaderBar::new();
    content_header.set_show_back_button(false);
    content_header.set_title_widget(Some(&content_title));
    content_header.pack_start(&sidebar_toggle);
    content_header.pack_start(&spinner);

    let content_toolbar = adw::ToolbarView::new();
    content_toolbar.add_top_bar(&content_header);
    content_toolbar.set_content(Some(&content_stack));

    let split_view = adw::OverlaySplitView::new();
    split_view.set_sidebar(Some(&sidebar_toolbar));
    split_view.set_content(Some(&content_toolbar));
    split_view.set_show_sidebar(true);
    split_view.set_enable_show_gesture(true);
    split_view.set_enable_hide_gesture(true);

    {
        let split_for_button = split_view.clone();
        sidebar_toggle.connect_clicked(move |_| {
            let shown = split_for_button.property::<bool>("show-sidebar");
            split_for_button.set_show_sidebar(!shown);
        });
    }
    {
        let button_for_state = sidebar_toggle.clone();
        split_view.connect_notify_local(Some("show-sidebar"), move |split, _| {
            let shown = split.property::<bool>("show-sidebar");
            button_for_state.set_tooltip_text(Some(if shown {
                "Hide Sidebar"
            } else {
                "Show Sidebar"
            }));
        });
    }

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("hardware-info-gtk4")
        .default_width(1080)
        .default_height(760)
        .content(&split_view)
        .build();

    // Overview arrow navigation is rebuilt around the actual visible card
    // surfaces. GTK still owns focus painting and Tab traversal.
    install_overview_keyboard_navigation(
        &window,
        overview_scroll.clone().upcast(),
        content_stack.clone(),
        cpu.card.clone().upcast(),
        gpu_cards.clone(),
        memory.card.clone().upcast(),
        disk_cards.clone(),
        battery_cards.clone(),
        temperature_rows.clone(),
        other_sensor_rows.clone(),
        sidebar_toggle.clone(),
        navigation.clone(),
    );

    let stack_for_navigation = content_stack.clone();
    let title_for_navigation = content_title.clone();
    let split_for_navigation = split_view.clone();
    navigation.connect_row_selected(move |_, row| {
        let Some(row) = row else {
            return;
        };
        let (name, title) = match row.index() {
            0 => ("overview", "Overview"),
            1 => ("devices", "Devices"),
            2 => ("hardware", "Hardware"),
            _ => return,
        };
        stack_for_navigation.set_visible_child_name(name);
        title_for_navigation.set_title(title);
        if split_for_navigation.property::<bool>("collapsed") {
            split_for_navigation.set_show_sidebar(false);
        }
    });

    let split_for_resize = split_view.clone();
    window.add_tick_callback(move |widget, _| {
        let collapsed = widget.width() < 760;
        if collapsed != split_for_resize.property::<bool>("collapsed") {
            split_for_resize.set_collapsed(collapsed);
        }
        glib::ControlFlow::Continue
    });

    add_window_action(&window, "close", {
        let window = window.clone();
        move || window.close()
    });
    add_window_action(&window, "toggle-sidebar", {
        let split_view = split_view.clone();
        move || {
            let shown = split_view.property::<bool>("show-sidebar");
            split_view.set_show_sidebar(!shown);
        }
    });
    add_window_action(&window, "overview", {
        let navigation = navigation.clone();
        let row = overview_nav.clone();
        move || navigation.select_row(Some(&row))
    });
    add_window_action(&window, "devices", {
        let navigation = navigation.clone();
        let row = devices_nav.clone();
        move || navigation.select_row(Some(&row))
    });
    add_window_action(&window, "hardware", {
        let navigation = navigation.clone();
        let row = hardware_nav.clone();
        move || navigation.select_row(Some(&row))
    });
    add_window_action(&window, "shortcuts", {
        let window = window.clone();
        move || present_shortcuts_dialog(&window)
    });
    add_window_action(&window, "about", {
        let window = window.clone();
        move || present_about_dialog(&window)
    });

    app.set_accels_for_action("win.close", &["<primary>w"]);
    app.set_accels_for_action("win.toggle-sidebar", &["F9"]);
    app.set_accels_for_action("win.shortcuts", &["<primary>question"]);
    app.set_accels_for_action("win.overview", &["<primary>1"]);
    app.set_accels_for_action("win.devices", &["<primary>2"]);
    app.set_accels_for_action("win.hardware", &["<primary>3"]);

    let ui = Ui {
        cpu,
        memory,
        performance_flow,
        gpu_cards,
        disk_flow,
        disk_section,
        disk_cards,
        battery_flow,
        battery_section,
        battery_cards,
        devices_list,
        device_rows,
        devices_stack,
        hardware_page,
        hardware_stack,
        hardware_initialized: Rc::new(Cell::new(false)),
        temperature_list,
        temperature_rows,
        other_sensors_list,
        other_sensor_rows,
        temperature_section,
        other_sensors_section,
        diagnostics_section,
        spinner,
    };

    glib::timeout_add_local(Duration::from_millis(150), move || {
        let mut latest = None;
        while let Ok(snapshot) = snapshot_rx.try_recv() {
            latest = Some(snapshot);
        }
        if let Some(snapshot) = latest {
            ui.update(&snapshot);
        }
        glib::ControlFlow::Continue
    });

    window.present();
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(APP_CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn metric_flow() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .css_classes(["metric-flow"])
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(false)
        .focusable(true)
        .min_children_per_line(1)
        .max_children_per_line(2)
        .row_spacing(12)
        .column_spacing(12)
        .homogeneous(true)
        .build()
}

fn metric_section(title: &str, flow: &gtk::FlowBox) -> gtk::Box {
    let title = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["title-2"])
        .build();
    let section = gtk::Box::new(Orientation::Vertical, 12);
    section.append(&title);
    section.append(flow);
    section
}

fn widget_is_within(widget: &gtk::Widget, ancestor: &gtk::Widget) -> bool {
    widget == ancestor || widget.is_ancestor(ancestor)
}

fn metric_focus_target(card: &gtk::Widget) -> gtk::Widget {
    card.parent()
        .filter(|parent| parent.is::<gtk::FlowBoxChild>())
        .unwrap_or_else(|| card.clone())
}

fn append_focus_target(items: &mut Vec<gtk::Widget>, widget: gtk::Widget) {
    if widget.is_visible() && widget.is_mapped() && widget.is_sensitive() && widget.is_focusable() {
        items.push(widget);
    }
}

fn append_metric_target(items: &mut Vec<gtk::Widget>, card: gtk::Widget) {
    append_focus_target(items, metric_focus_target(&card));
}

fn target_center(widget: &gtk::Widget, root: &gtk::Widget) -> Option<(f32, f32)> {
    let bounds = widget.compute_bounds(root)?;
    Some((
        bounds.x() + bounds.width() / 2.0,
        bounds.y() + bounds.height() / 2.0,
    ))
}

fn spatial_target(
    items: &[gtk::Widget],
    current_index: usize,
    root: &gtk::Widget,
    key: gtk::gdk::Key,
) -> Option<usize> {
    let (current_x, current_y) = target_center(&items[current_index], root)?;
    let mut best: Option<(usize, f32)> = None;

    for (index, candidate) in items.iter().enumerate() {
        if index == current_index {
            continue;
        }
        let Some((candidate_x, candidate_y)) = target_center(candidate, root) else {
            continue;
        };

        let dx = candidate_x - current_x;
        let dy = candidate_y - current_y;
        let (primary, cross) = if key == gtk::gdk::Key::Right && dx > 4.0 {
            (dx, dy.abs())
        } else if key == gtk::gdk::Key::Left && dx < -4.0 {
            (-dx, dy.abs())
        } else if key == gtk::gdk::Key::Down && dy > 4.0 {
            (dy, dx.abs())
        } else if key == gtk::gdk::Key::Up && dy < -4.0 {
            (-dy, dx.abs())
        } else {
            continue;
        };

        // Prefer the nearest item in the requested direction. Crossing into a
        // different row or section is allowed, but lateral drift is penalized.
        let score = primary + cross * 3.0;
        if best.map(|(_, best_score)| score < best_score).unwrap_or(true) {
            best = Some((index, score));
        }
    }

    best.map(|(index, _)| index)
}

#[allow(clippy::too_many_arguments)]
fn install_overview_keyboard_navigation(
    window: &adw::ApplicationWindow,
    overview_root: gtk::Widget,
    content_stack: gtk::Stack,
    cpu: gtk::Widget,
    gpu_cards: Rc<RefCell<Vec<(String, MetricCard)>>>,
    memory: gtk::Widget,
    disk_cards: Rc<RefCell<Vec<(String, MetricCard)>>>,
    battery_cards: Rc<RefCell<Vec<(String, MetricCard)>>>,
    temperature_rows: Rc<RefCell<Vec<SensorRowUi>>>,
    other_sensor_rows: Rc<RefCell<Vec<SensorRowUi>>>,
    sidebar_toggle: gtk::Button,
    navigation: gtk::ListBox,
) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    // Keep a logical Overview cursor independently from GTK's focus-visible
    // state. GTK may hide the focus ring after pointer activity or inactivity,
    // but arrow navigation should resume from the last card reached by Tab or
    // by an arrow key.
    let last_active = Rc::new(Cell::new(None::<usize>));
    let navigation_armed = Rc::new(Cell::new(false));

    let last_active_for_focus = last_active.clone();
    let navigation_armed_for_focus = navigation_armed.clone();
    let overview_root_for_focus = overview_root.clone();
    let content_stack_for_focus = content_stack.clone();
    let cpu_for_focus = cpu.clone();
    let gpu_cards_for_focus = gpu_cards.clone();
    let memory_for_focus = memory.clone();
    let disk_cards_for_focus = disk_cards.clone();
    let battery_cards_for_focus = battery_cards.clone();
    let temperature_rows_for_focus = temperature_rows.clone();
    let other_sensor_rows_for_focus = other_sensor_rows.clone();
    window.connect_notify_local(Some("focus-widget"), move |window, _| {
        if content_stack_for_focus.visible_child_name().as_deref() != Some("overview") {
            navigation_armed_for_focus.set(false);
            return;
        }

        // A temporarily missing root focus must not erase the logical cursor.
        let Some(focused) = gtk::prelude::RootExt::focus(window) else {
            return;
        };

        let mut items = Vec::new();
        append_metric_target(&mut items, cpu_for_focus.clone());
        for (_, card) in gpu_cards_for_focus.borrow().iter() {
            append_metric_target(&mut items, card.card.clone().upcast());
        }
        append_metric_target(&mut items, memory_for_focus.clone());
        for (_, card) in disk_cards_for_focus.borrow().iter() {
            append_metric_target(&mut items, card.card.clone().upcast());
        }
        for (_, card) in battery_cards_for_focus.borrow().iter() {
            append_metric_target(&mut items, card.card.clone().upcast());
        }
        for row in temperature_rows_for_focus.borrow().iter() {
            append_focus_target(&mut items, row.row.clone().upcast());
        }
        for row in other_sensor_rows_for_focus.borrow().iter() {
            append_focus_target(&mut items, row.row.clone().upcast());
        }

        if let Some(index) = items.iter().position(|item| widget_is_within(&focused, item)) {
            last_active_for_focus.set(Some(index));
            navigation_armed_for_focus.set(true);
        } else if !widget_is_within(&focused, &overview_root_for_focus)
            && focused.is_focusable()
        {
            // Explicitly moving to another real control (sidebar, menu, etc.)
            // ends Overview arrow navigation. A transient non-focusable/root
            // widget used while GTK hides focus visibility does not.
            navigation_armed_for_focus.set(false);
        }
    });

    let window_for_keys = window.clone();
    let last_active_for_keys = last_active.clone();
    let navigation_armed_for_keys = navigation_armed.clone();
    let sidebar_toggle_for_keys = sidebar_toggle.clone();
    let navigation_for_keys = navigation.clone();
    controller.connect_key_pressed(move |_, key, _, modifiers| {
        if modifiers.intersects(
            gtk::gdk::ModifierType::CONTROL_MASK
                | gtk::gdk::ModifierType::ALT_MASK
                | gtk::gdk::ModifierType::SUPER_MASK,
        ) {
            return glib::Propagation::Proceed;
        }

        if content_stack.visible_child_name().as_deref() != Some("overview") {
            return glib::Propagation::Proceed;
        }

        let focused = gtk::prelude::RootExt::focus(&window_for_keys);
        let focused_in_overview = focused
            .as_ref()
            .is_some_and(|widget| widget_is_within(widget, &overview_root));
        let tab = key == gtk::gdk::Key::Tab || key == gtk::gdk::Key::ISO_Left_Tab;
        if tab && (focused_in_overview || navigation_armed_for_keys.get()) {
            let backwards = key == gtk::gdk::Key::ISO_Left_Tab
                || modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);

            gtk::prelude::GtkWindowExt::set_focus_visible(&window_for_keys, true);
            let moved = if backwards {
                navigation_for_keys
                    .selected_row()
                    .filter(|row| row.is_visible() && row.is_mapped())
                    .is_some_and(|row| row.grab_focus())
                    || sidebar_toggle_for_keys.grab_focus()
            } else {
                sidebar_toggle_for_keys.grab_focus()
            };

            if moved {
                navigation_armed_for_keys.set(false);
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }

        let directional = key == gtk::gdk::Key::Up
            || key == gtk::gdk::Key::Down
            || key == gtk::gdk::Key::Left
            || key == gtk::gdk::Key::Right;
        let home_or_end = key == gtk::gdk::Key::Home || key == gtk::gdk::Key::End;
        if !directional && !home_or_end {
            return glib::Propagation::Proceed;
        }

        let mut items = Vec::new();
        append_metric_target(&mut items, cpu.clone());
        for (_, card) in gpu_cards.borrow().iter() {
            append_metric_target(&mut items, card.card.clone().upcast());
        }
        append_metric_target(&mut items, memory.clone());
        for (_, card) in disk_cards.borrow().iter() {
            append_metric_target(&mut items, card.card.clone().upcast());
        }
        for (_, card) in battery_cards.borrow().iter() {
            append_metric_target(&mut items, card.card.clone().upcast());
        }
        for row in temperature_rows.borrow().iter() {
            append_focus_target(&mut items, row.row.clone().upcast());
        }
        for row in other_sensor_rows.borrow().iter() {
            append_focus_target(&mut items, row.row.clone().upcast());
        }
        if items.is_empty() {
            return glib::Propagation::Proceed;
        }

        let current_from_focus = focused.as_ref().and_then(|widget| {
            items.iter().position(|item| widget_is_within(widget, item))
        });

        if let Some(index) = current_from_focus {
            last_active_for_keys.set(Some(index));
            navigation_armed_for_keys.set(true);
        }

        // Never steal arrows from an explicitly focused sidebar/menu control.
        // A transient non-focusable widget outside Overview is treated like a
        // missing focus and resumes from the remembered card instead.
        if focused
            .as_ref()
            .is_some_and(|widget| !widget_is_within(widget, &overview_root) && widget.is_focusable())
        {
            return glib::Propagation::Proceed;
        }

        // Arrow navigation is activated only after a card was reached with Tab
        // or an arrow key. Once activated, a transiently hidden/dropped GTK
        // focus does not interrupt it; the remembered logical cursor is used.
        if !navigation_armed_for_keys.get() {
            return glib::Propagation::Proceed;
        }

        let current = current_from_focus
            .or_else(|| last_active_for_keys.get().filter(|index| *index < items.len()))
            .unwrap_or(0);

        let spatial = if directional {
            spatial_target(&items, current, &overview_root, key)
        } else {
            None
        };

        // Do not trap focus at the top or left edge of Overview. Moving left
        // exits to the selected sidebar destination when the sidebar is shown;
        // moving up exits to the sidebar show/hide button in the header.
        if spatial.is_none() && (key == gtk::gdk::Key::Left || key == gtk::gdk::Key::Up) {
            gtk::prelude::GtkWindowExt::set_focus_visible(&window_for_keys, true);
            let moved = if key == gtk::gdk::Key::Left {
                navigation_for_keys
                    .selected_row()
                    .filter(|row| row.is_visible() && row.is_mapped())
                    .is_some_and(|row| row.grab_focus())
                    || sidebar_toggle_for_keys.grab_focus()
            } else {
                sidebar_toggle_for_keys.grab_focus()
            };
            if moved {
                navigation_armed_for_keys.set(false);
                return glib::Propagation::Stop;
            }
        }

        let target = if key == gtk::gdk::Key::Home {
            Some(0)
        } else if key == gtk::gdk::Key::End {
            Some(items.len() - 1)
        } else {
            spatial.or_else(|| {
                if (key == gtk::gdk::Key::Down || key == gtk::gdk::Key::Right)
                    && current + 1 < items.len()
                {
                    Some(current + 1)
                } else if (key == gtk::gdk::Key::Up || key == gtk::gdk::Key::Left)
                    && current > 0
                {
                    Some(current - 1)
                } else {
                    Some(current)
                }
            })
        };

        let Some(index) = target else {
            return glib::Propagation::Stop;
        };

        // Focus visibility is a GtkWindow property, not a Widget property.
        // Restore it on the application window, then move focus to the card.
        gtk::prelude::GtkWindowExt::set_focus_visible(&window_for_keys, true);
        if items[index].grab_focus() {
            gtk::prelude::GtkWindowExt::set_focus_visible(&window_for_keys, true);
            last_active_for_keys.set(Some(index));
            navigation_armed_for_keys.set(true);
            return glib::Propagation::Stop;
        }

        glib::Propagation::Proceed
    });

    window.add_controller(controller);
}

fn navigation_row(icon_name: &str, title: &str) -> gtk::ListBoxRow {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(18);
    navigation_row_with_icon(icon.upcast::<gtk::Widget>(), title)
}

fn navigation_row_with_icon(icon: gtk::Widget, title: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(true)
        .build();

    let content = gtk::Box::new(Orientation::Horizontal, 12);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&icon);
    content.append(&label);

    let row = gtk::ListBoxRow::builder().child(&content).build();
    row.set_focusable(true);
    row
}

fn add_window_action<F>(window: &adw::ApplicationWindow, name: &str, callback: F)
where
    F: Fn() + 'static,
{
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| callback());
    window.add_action(&action);
}

fn present_shortcuts_dialog(parent: &adw::ApplicationWindow) {
    let navigation_group = adw::PreferencesGroup::builder().title("Navigation").build();
    navigation_group.add(&shortcut_row("Show Overview", "<Primary>1"));
    navigation_group.add(&shortcut_row("Show Devices", "<Primary>2"));
    navigation_group.add(&shortcut_row("Show Hardware", "<Primary>3"));
    navigation_group.add(&shortcut_row("Show or Hide Sidebar", "F9"));
    navigation_group.add(&shortcut_row("Move to Next Item", "Tab"));
    navigation_group.add(&shortcut_row("Move to Previous Item", "<Shift>Tab"));
    navigation_group.add(&shortcut_row("Move Through Overview Items", "Up Down Left Right"));
    navigation_group.add(&shortcut_row("Jump to First or Last Overview Item", "Home End"));
    navigation_group.add(&shortcut_row("Activate a Focused Control", "Return space"));

    let application_group = adw::PreferencesGroup::builder().title("Application").build();
    application_group.add(&shortcut_row("Show Keyboard Shortcuts", "<Primary>question"));
    application_group.add(&shortcut_row("Close Window", "<Primary>w"));
    application_group.add(&shortcut_row("Quit Application", "<Primary>q"));

    let page = adw::PreferencesPage::new();
    page.add(&navigation_group);
    page.add(&application_group);

    let title = adw::WindowTitle::builder()
        .title("Keyboard Shortcuts")
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));

    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(500)
        .content_height(480)
        .child(&toolbar)
        .build();
    dialog.present(Some(parent));
}

fn shortcut_row(title: &str, accelerator: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::ShortcutLabel::builder()
        .accelerator(accelerator)
        .valign(Align::Center)
        .build();
    row.add_suffix(&label);
    row
}

fn present_about_dialog(parent: &adw::ApplicationWindow) {
    let dialog = adw::AboutDialog::builder()
        .application_name("hardware-info-gtk4")
        .developer_name("Mars7x (via GPT-5.6 Sol)")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A focused Linux hardware telemetry monitor built with GTK 4 and libadwaita.")
        .license("No license")
        .build();
    dialog.present(Some(parent));
}

fn populate_hardware_inventory(page: &adw::PreferencesPage, snapshot: &Snapshot) {
    let computer_group = adw::PreferencesGroup::builder().title("Computer").build();
    let mut computer_rows = 0;

    if is_meaningful_text(&snapshot.static_info.system) {
        computer_group.add(&inventory_row(
            "computer-symbolic",
            &snapshot.static_info.system,
            "System model",
        ));
        computer_rows += 1;
    }
    if is_meaningful_text(&snapshot.static_info.cpu_model) {
        let cpu_details = if is_meaningful_text(&snapshot.static_info.cpu_topology) {
            snapshot.static_info.cpu_topology.as_str()
        } else {
            meaningful_or_empty(&snapshot.static_info.architecture)
        };
        computer_group.add(&inventory_row(
            "processor-symbolic",
            &snapshot.static_info.cpu_model,
            cpu_details,
        ));
        computer_rows += 1;
    }
    if snapshot.memory_total_bytes > 0 {
        computer_group.add(&inventory_row(
            "media-flash-symbolic",
            &format!("{} memory", format_bytes(snapshot.memory_total_bytes)),
            "Installed system memory",
        ));
        computer_rows += 1;
    }
    if is_meaningful_text(&snapshot.static_info.motherboard) {
        computer_group.add(&inventory_row(
            "preferences-system-symbolic",
            &snapshot.static_info.motherboard,
            "Motherboard",
        ));
        computer_rows += 1;
    }
    if is_meaningful_text(&snapshot.static_info.bios) {
        computer_group.add(&inventory_row(
            "security-high-symbolic",
            &snapshot.static_info.bios,
            "System firmware",
        ));
        computer_rows += 1;
    }
    if computer_rows > 0 {
        page.add(&computer_group);
    }

    let graphics = if snapshot.static_info.graphics.is_empty() {
        snapshot
            .gpus
            .iter()
            .filter_map(|gpu| {
                let name = concise_gpu_name(&gpu.name);
                if !is_meaningful_text(&name) {
                    return None;
                }
                let mut details = Vec::new();
                if let Some(board_name) = gpu
                    .board_name
                    .as_deref()
                    .filter(|value| is_meaningful_text(value))
                {
                    details.push(board_name.to_string());
                }
                if let Some(value) = gpu.memory_total_bytes.filter(|value| *value > 0) {
                    details.push(format!("{} video memory", format_bytes(value)));
                }
                Some(ComponentInfo {
                    name,
                    details: details.join(" · "),
                })
            })
            .collect::<Vec<_>>()
    } else {
        snapshot.static_info.graphics.clone()
    };
    add_component_group(page, "Graphics", themed_icon_name("gpu-symbolic", "video-display-symbolic"), &graphics);
    add_component_group(
        page,
        "Storage devices",
        "drive-harddisk-symbolic",
        &snapshot.static_info.storage,
    );
    add_component_group(
        page,
        "Batteries",
        "battery-symbolic",
        &snapshot.static_info.batteries,
    );
    add_component_group(
        page,
        "USB devices",
        "drive-removable-media-symbolic",
        &snapshot.static_info.usb_devices,
    );
    add_component_group(
        page,
        "PCI devices",
        "preferences-system-symbolic",
        &snapshot.static_info.pci_devices,
    );
}

fn add_component_group(
    page: &adw::PreferencesPage,
    title: &str,
    icon_name: &str,
    components: &[ComponentInfo],
) {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let mut count = 0;
    for component in components {
        if !is_meaningful_text(&component.name) {
            continue;
        }
        group.add(&inventory_row(
            icon_name,
            component.name.trim(),
            meaningful_or_empty(&component.details),
        ));
        count += 1;
    }
    if count > 0 {
        page.add(&group);
    }
}

fn inventory_row(icon_name: &str, title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.set_activatable(false);
    row.set_focusable(true);
    row.set_title_selectable(true);
    row.set_subtitle_selectable(true);

    let icon = app_icon_image(icon_name, 18);
    icon.set_valign(Align::Center);
    icon.add_css_class("dim-label");
    row.add_prefix(&icon);
    row
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .focusable(false)
        .css_classes(["boxed-list"])
        .build()
}

fn list_section(title: &str, description: &str, list: &gtk::ListBox) -> gtk::Box {
    let title = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["title-3"])
        .build();
    let description = gtk::Label::builder()
        .label(description)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();

    let section = gtk::Box::new(Orientation::Vertical, 8);
    section.append(&title);
    section.append(&description);
    section.append(list);
    section
}

fn temperature_displays(snapshot: &Snapshot) -> Vec<SensorDisplay> {
    let categories = [
        "Memory temperature",
        "Motherboard temperature",
    ];
    let mut displays = Vec::new();

    for category in categories {
        if let Some(sensor) = snapshot
            .temperatures
            .iter()
            .filter(|sensor| sensor.kind == category)
            .max_by(|left, right| left.value.total_cmp(&right.value))
        {
            displays.push(SensorDisplay {
                key: format!("temperature:{category}"),
                title: readable_temperature_category(category).to_string(),
                subtitle: concise_sensor_name(sensor),
                value: format!("{:.1} °C", sensor.value),
                icon_name: "power-profile-balanced-symbolic".to_string(),
            });
        }
    }

    displays
}

fn other_sensor_displays(snapshot: &Snapshot) -> Vec<SensorDisplay> {
    let mut displays = Vec::new();

    let mut fans = snapshot
        .fans
        .iter()
        .filter(|sensor| sensor.value > 0.0)
        .collect::<Vec<_>>();
    fans.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    for (index, fan) in fans.into_iter().take(6).enumerate() {
        displays.push(SensorDisplay {
            key: format!("fan:{index}:{}:{}", fan.kind, fan.name),
            title: concise_sensor_name(fan),
            subtitle: "Fan speed".to_string(),
            value: format!("{:.0} RPM", fan.value),
            icon_name: "weather-windy-symbolic".to_string(),
        });
    }

    let mut power = snapshot
        .power
        .iter()
        .filter(|sensor| {
            let name = sensor.name.to_ascii_lowercase();
            sensor.value > 0.05
                && sensor.value < 5_000.0
                && !name.contains("amdgpu")
                && !name.contains("nvidia")
                && !name.contains("nouveau")
        })
        .collect::<Vec<_>>();
    power.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    for (index, sensor) in power.into_iter().take(6).enumerate() {
        displays.push(SensorDisplay {
            key: format!("power:{index}:{}:{}", sensor.kind, sensor.name),
            title: concise_sensor_name(sensor),
            subtitle: "Power draw".to_string(),
            value: format!("{:.1} W", sensor.value),
            icon_name: "power-profile-balanced-symbolic".to_string(),
        });
    }

    displays
}

fn sync_sensor_rows(
    list: &gtk::ListBox,
    rows: &Rc<RefCell<Vec<SensorRowUi>>>,
    displays: Vec<SensorDisplay>,
) -> usize {
    let keys = displays
        .iter()
        .map(|display| display.key.clone())
        .collect::<Vec<_>>();
    let old_keys = rows
        .borrow()
        .iter()
        .map(|row| row.key.clone())
        .collect::<Vec<_>>();

    if keys != old_keys {
        let focused_key = rows
            .borrow()
            .iter()
            .find(|row| row.row.has_focus())
            .map(|row| row.key.clone());

        clear_list(list);
        let mut new_rows = Vec::new();
        for display in &displays {
            let row = create_sensor_row(display);
            list.append(&row.row);
            new_rows.push(row);
        }
        *rows.borrow_mut() = new_rows;

        if let Some(focused_key) = focused_key {
            if let Some(row) = rows
                .borrow()
                .iter()
                .find(|row| row.key == focused_key)
            {
                let _ = row.row.grab_focus();
            }
        }
    }

    for (row, display) in rows.borrow().iter().zip(&displays) {
        row.row.set_title(&display.title);
        row.row.set_subtitle(&display.subtitle);
        row.value.set_label(&display.value);
    }

    displays.len()
}

fn create_sensor_row(display: &SensorDisplay) -> SensorRowUi {
    let row = adw::ActionRow::builder()
        .title(display.title.as_str())
        .subtitle(display.subtitle.as_str())
        .build();
    row.set_activatable(false);
    row.set_focusable(true);

    let icon = gtk::Image::from_icon_name(display.icon_name.as_str());
    icon.add_css_class("dim-label");
    row.add_prefix(&icon);

    let value = gtk::Label::builder()
        .label(display.value.as_str())
        .valign(Align::Center)
        .xalign(1.0)
        .css_classes(["numeric"])
        .build();
    row.add_suffix(&value);

    SensorRowUi {
        key: display.key.clone(),
        row,
        value,
    }
}

fn sync_device_rows(
    list: &gtk::ListBox,
    rows: &Rc<RefCell<Vec<DeviceRowUi>>>,
    devices: &[DeviceInfo],
) -> usize {
    let keys = devices.iter().map(|device| device.key.clone()).collect::<Vec<_>>();
    let old_keys = rows
        .borrow()
        .iter()
        .map(|row| row.key.clone())
        .collect::<Vec<_>>();

    if keys != old_keys {
        clear_list(list);
        let mut new_rows = Vec::new();
        for device in devices {
            let row = create_device_row(device);
            list.append(&row.row);
            new_rows.push(row);
        }
        *rows.borrow_mut() = new_rows;
    }

    for (row, device) in rows.borrow().iter().zip(devices) {
        row.row.set_title(&device.name);
        row.row.set_subtitle(device.transport.trim());
        row.icon.set_icon_name(Some(device_connection_icon_name(device)));
        set_device_battery(row, device.battery_percent);
    }

    devices.len()
}

fn create_device_row(device: &DeviceInfo) -> DeviceRowUi {
    let row = adw::ActionRow::builder()
        .title(device.name.as_str())
        .build();
    row.set_activatable(false);
    row.set_focusable(true);
    row.set_title_selectable(true);
    row.set_subtitle_selectable(true);

    let icon = device_connection_icon(device);
    row.add_prefix(&icon);

    let battery_label = gtk::Label::builder()
        .xalign(1.0)
        .css_classes(["numeric"])
        .build();
    let battery_bar = gtk::ProgressBar::builder()
        .show_text(false)
        .valign(Align::Center)
        .build();
    battery_bar.set_size_request(92, -1);
    let battery_box = gtk::Box::new(Orientation::Horizontal, 8);
    battery_box.set_valign(Align::Center);
    battery_box.append(&battery_label);
    battery_box.append(&battery_bar);
    row.add_suffix(&battery_box);

    let result = DeviceRowUi {
        key: device.key.clone(),
        row,
        icon,
        battery_box,
        battery_label,
        battery_bar,
    };
    set_device_battery(&result, device.battery_percent);
    result
}

fn set_device_battery(row: &DeviceRowUi, percentage: Option<f64>) {
    for class in [
        "device-battery-high",
        "device-battery-medium",
        "device-battery-low",
    ] {
        row.battery_bar.remove_css_class(class);
    }

    let Some(percentage) = percentage.filter(|value| value.is_finite()) else {
        row.battery_box.set_visible(false);
        return;
    };
    let whole = percentage.clamp(0.0, 100.0).round();
    row.battery_label.set_label(&format!("{whole:.0}%"));
    row.battery_bar.set_fraction(whole / 100.0);
    row.battery_bar.add_css_class(if whole >= 51.0 {
        "device-battery-high"
    } else if whole >= 30.0 {
        "device-battery-medium"
    } else {
        "device-battery-low"
    });
    row.battery_box.set_visible(true);
}

fn device_connection_icon_name(device: &DeviceInfo) -> &'static str {
    if device.transport.to_ascii_lowercase().contains("bluetooth") {
        "bluetooth-symbolic"
    } else {
        "drive-harddisk-usb-symbolic"
    }
}

fn device_connection_icon(device: &DeviceInfo) -> gtk::Image {
    let icon = gtk::Image::from_icon_name(device_connection_icon_name(device));
    icon.set_pixel_size(20);
    icon.add_css_class("dim-label");
    icon.set_can_target(false);
    icon
}

fn app_icon_image(icon_name: &str, pixel_size: i32) -> gtk::Image {
    if matches!(icon_name, "gpu-symbolic" | "processor-symbolic") {
        if let Some(path) = packaged_symbolic_icon_path(icon_name) {
            let file = gio::File::for_path(path);
            let paintable = gtk::IconPaintable::for_file(&file, pixel_size, 1);
            // IconPaintable recognizes the -symbolic.svg suffix and exposes
            // a symbolic paintable, allowing GTK to recolor it for the theme.
            let image = gtk::Image::from_paintable(Some(&paintable));
            image.set_pixel_size(pixel_size);
            return image;
        }
    }

    let image = gtk::Image::from_icon_name(icon_name);
    image.set_pixel_size(pixel_size);
    image
}

fn packaged_symbolic_icon_path(icon_name: &str) -> Option<PathBuf> {
    let file_name = format!("{icon_name}.svg");
    let mut candidates = Vec::new();

    // Installed native and Flatpak layouts: <prefix>/bin and <prefix>/share.
    if let Ok(executable) = std::env::current_exe() {
        if let Some(prefix) = executable.parent().and_then(|bin| bin.parent()) {
            candidates.push(
                prefix
                    .join("share/hardware-info-gtk4/icons")
                    .join(&file_name),
            );
        }
    }

    // Cargo development builds run directly from the source tree.
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data/icons/hicolor/symbolic/apps")
            .join(&file_name),
    );

    // Conventional system locations cover manually installed binaries.
    for prefix in ["/app", "/usr/local", "/usr"] {
        candidates.push(
            PathBuf::from(prefix)
                .join("share/hardware-info-gtk4/icons")
                .join(&file_name),
        );
    }

    candidates.into_iter().find(|path| path.is_file())
}

fn themed_icon_name(preferred: &'static str, fallback: &'static str) -> &'static str {
    let Some(display) = gtk::gdk::Display::default() else {
        return fallback;
    };
    if gtk::IconTheme::for_display(&display).has_icon(preferred) {
        preferred
    } else {
        fallback
    }
}

fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn insert_metric_card(flow: &gtk::FlowBox, card: &gtk::Box, position: i32) {
    flow.insert(card, position);
    if let Some(wrapper) = card.parent() {
        // Put the visible libadwaita card surface on the focusable FlowBoxChild
        // itself. The native theme can then paint an unclipped focus indicator
        // in both one-column and multi-column layouts.
        wrapper.add_css_class("card");
        wrapper.add_css_class("metric-card");
        wrapper.set_focusable(true);
        wrapper.set_focus_on_click(false);
        // Informational cards should never react to pointer hover or clicks.
        // can-target affects pointer picking only; keyboard focus remains enabled.
        wrapper.set_can_target(false);
    }
}

fn clear_flow(flow: &gtk::FlowBox) {
    while let Some(child) = flow.first_child() {
        flow.remove(&child);
    }
}

fn cpu_accent(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.contains("amd") || lower.contains("ryzen") || lower.contains("epyc") {
        "cpu-amd"
    } else if lower.contains("intel") || lower.contains("xeon") || lower.contains("core(tm)") {
        "cpu-intel"
    } else {
        "neutral"
    }
}

fn gpu_accent(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("nvidia") || lower.contains("geforce") || lower.contains("quadro") {
        "gpu-nvidia"
    } else if lower.contains("amd")
        || lower.contains("radeon")
        || lower.contains("ati")
        || lower.contains("advanced micro devices")
    {
        "gpu-amd"
    } else if lower.contains("intel") {
        "gpu-intel"
    } else {
        "gpu-neutral"
    }
}

fn disk_title(disk: &DiskUsage) -> String {
    if is_meaningful_text(&disk.name) {
        return disk.name.trim().to_string();
    }

    let device = disk.device.trim_start_matches("/dev/");
    if device.starts_with("nvme") {
        "NVMe drive".to_string()
    } else if device.starts_with("mmcblk") {
        "eMMC storage".to_string()
    } else if device.is_empty() {
        "Storage drive".to_string()
    } else {
        format!("Drive {device}")
    }
}

fn concise_sensor_name(sensor: &SensorReading) -> String {
    sensor
        .name
        .replace("Package id 0", "Package")
        .replace("Composite", "Drive")
        .replace("temp1", "Temperature")
}

fn concise_cpu_name(name: &str) -> String {
    let mut value = name
        .replace("(R)", "")
        .replace("(TM)", "")
        .replace("®", "")
        .replace("™", "");

    if let Some((before, _)) = value.split_once(" with Radeon") {
        value = before.to_string();
    }
    if let Some((before, _)) = value.split_once(" @ ") {
        value = before.to_string();
    }

    let words = value
        .split_whitespace()
        .filter(|word| {
            let lower = word.to_ascii_lowercase();
            lower != "processor"
                && lower != "cpu"
                && !lower.ends_with("-core")
                && lower != "authenticamd"
        })
        .collect::<Vec<_>>();
    let mut value = words.join(" ");
    for prefix in ["AMD Ryzen ", "AMD EPYC ", "AMD Athlon "] {
        if let Some(rest) = value.strip_prefix(prefix) {
            value = format!("{} {}", prefix.split_whitespace().nth(1).unwrap_or("AMD"), rest);
            break;
        }
    }
    collapse_whitespace(&value)
}

fn concise_gpu_name(name: &str) -> String {
    let trimmed = name.trim();
    let lower = trimmed.to_ascii_lowercase();
    let without_class = if lower.starts_with("vga compatible controller")
        || lower.starts_with("3d controller")
        || lower.starts_with("display controller")
    {
        trimmed
            .split_once(": ")
            .map(|(_, device)| device)
            .unwrap_or(trimmed)
    } else {
        trimmed
    };

    let candidates = bracket_contents(without_class);
    if let Some(marketing_name) = candidates.into_iter().rev().find(|candidate| {
        let lower = candidate.to_ascii_lowercase();
        [
            "radeon", "geforce", "rtx", "gtx", "quadro", "tesla", "arc ",
            "iris", "uhd graphics", "hd graphics",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    }) {
        let lower = marketing_name.to_ascii_lowercase();
        let vendor = if lower.contains("radeon") {
            "AMD"
        } else if lower.contains("geforce")
            || lower.contains("rtx")
            || lower.contains("gtx")
            || lower.contains("quadro")
            || lower.contains("tesla")
        {
            "NVIDIA"
        } else if lower.contains("arc")
            || lower.contains("iris")
            || lower.contains("uhd")
            || lower.contains("hd graphics")
        {
            "Intel"
        } else {
            ""
        };
        let cleaned = marketing_name
            .replace("AMD ", "")
            .replace("NVIDIA ", "")
            .replace("Intel ", "");
        return collapse_whitespace(&format!("{vendor} {cleaned}"));
    }

    let mut value = strip_trailing_pci_ids(without_class);
    for (from, to) in [
        ("Advanced Micro Devices, Inc. [AMD/ATI]", "AMD"),
        ("Advanced Micro Devices, Inc.", "AMD"),
        ("NVIDIA Corporation", "NVIDIA"),
        ("Intel Corporation", "Intel"),
        ("[AMD/ATI]", ""),
    ] {
        value = value.replace(from, to);
    }
    collapse_whitespace(&value)
}

fn bracket_contents(value: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('[') {
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find(']') else {
            break;
        };
        result.push(after_start[..end].trim().to_string());
        rest = &after_start[end + 1..];
    }
    result
}

fn strip_trailing_pci_ids(value: &str) -> String {
    let mut result = value.trim().to_string();
    loop {
        let Some((prefix, suffix)) = result.rsplit_once(" [") else {
            break;
        };
        let id = suffix.trim_end_matches(']');
        let is_id = !id.is_empty()
            && id
                .chars()
                .all(|character| character.is_ascii_hexdigit() || character == ':');
        if !is_id {
            break;
        }
        result = prefix.trim().to_string();
    }
    result
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_meaningful_text(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    let exact_placeholders = [
        "unknown",
        "unknown device",
        "unknown processor",
        "unavailable",
        "not available",
        "not applicable",
        "n/a",
        "none",
        "not specified",
        "undefined",
        "invalid",
        "device",
        "graphics adapter",
        "usb device",
    ];
    let embedded_placeholders = [
        "default string",
        "system product name",
        "system manufacturer",
        "unknown system",
        "to be filled by o.e.m.",
        "to be filled by oem",
        "oem default",
    ];
    !exact_placeholders.contains(&lower.as_str())
        && !embedded_placeholders
            .iter()
            .any(|placeholder| lower.contains(placeholder))
}

fn meaningful_or_empty(value: &str) -> &str {
    if is_meaningful_text(value) {
        value.trim()
    } else {
        ""
    }
}

fn readable_temperature_category(category: &str) -> &'static str {
    match category {
        "CPU temperature" => "Processor",
        "GPU temperature" => "Graphics",
        "Memory temperature" => "Memory",
        "Storage temperature" => "Storage",
        _ => "Motherboard",
    }
}

fn ratio(used: u64, total: u64) -> Option<f64> {
    (total > 0).then_some(used as f64 / total as f64)
}

fn format_frequency(mhz: f64) -> String {
    if mhz >= 1_000.0 {
        format!("{:.2} GHz", mhz / 1_000.0)
    } else {
        format!("{mhz:.0} MHz")
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
