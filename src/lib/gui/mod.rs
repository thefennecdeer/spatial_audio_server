use atomic::Atomic;
use audio;
use cgmath;
use config::Config;
use conrod::{self, color, text, widget, Borderable, Colorable, FontSize, Labelable, Positionable,
             Scalar, Sizeable, UiBuilder, UiCell, Widget};
use conrod::backend::glium::{glium, Renderer};
use conrod::event::Input;
use conrod::render::OwnedPrimitives;
use image;
use interaction::Interaction;
use metres::Metres;
use rosc::OscMessage;
use std;
use std::sync::atomic;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::sync::{mpsc, Arc};
use std::sync::atomic::AtomicUsize;

mod theme;

/// A convenience wrapper that borrows the GUI state necessary for instantiating the widgets.
struct Gui<'a> {
    ui: UiCell<'a>,
    /// The images used throughout the GUI.
    images: &'a Images,
    fonts: &'a Fonts,
    ids: &'a mut Ids,
    state: &'a mut State,
}

/// Messages received by the GUI thread.
pub enum Message {
    Osc(SocketAddr, OscMessage),
    Interaction(Interaction),
    Input(Input),
}

struct State {
    // The loaded config file.
    config: Config,
    // The camera over the 2D floorplan.
    camera: Camera,
    // A log of the most recently received OSC messages for testing/debugging/monitoring.
    osc_log: OscLog,
    // A log of the most recently received Interactions for testing/debugging/monitoring.
    interaction_log: InteractionLog,
    speaker_editor: SpeakerEditor,
    // Menu states.
    side_menu_is_open: bool,
    osc_log_is_open: bool,
    interaction_log_is_open: bool,
}

struct SpeakerEditor {
    is_open: bool,
    // The list of speaker outputs.
    speakers: Vec<Speaker>,
    // The index of the selected speaker.
    selected: Option<usize>,
    // The next ID to be used for a new speaker.
    next_id: audio::SpeakerId,
    // A channel for adding/removing speakers.
    audio_msg_tx: mpsc::Sender<audio::Message>,
}

struct Speaker {
    // Speaker state shared with the audio thread.
    audio: Arc<audio::Speaker>,
    name: String,
    id: audio::SpeakerId,
}

impl<'a> Deref for Gui<'a> {
    type Target = UiCell<'a>;
    fn deref(&self) -> &Self::Target {
        &self.ui
    }
}

impl<'a> DerefMut for Gui<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ui
    }
}

type ImageMap = conrod::image::Map<glium::texture::Texture2d>;

#[derive(Clone, Copy, Debug)]
struct Image {
    id: conrod::image::Id,
    width: Scalar,
    height: Scalar,
}

#[derive(Debug)]
struct Images {
    floorplan: Image,
}

#[derive(Debug)]
struct Fonts {
    notosans_regular: text::font::Id,
}

// A 2D camera used to navigate around the floorplan visualisation.
#[derive(Debug)]
struct Camera {
    // The number of floorplan pixels per metre.
    floorplan_pixels_per_metre: f64,
    // The position of the camera over the floorplan.
    //
    // [0.0, 0.0] - the centre of the floorplan.
    position: cgmath::Point2<Metres>,
    // The higher the zoom, the closer the floorplan appears.
    //
    // The zoom can be multiplied by a distance in metres to get the equivalent distance as a GUI
    // scalar value.
    //
    // 1.0 - Original resolution.
    // 0.5 - 50% view.
    zoom: Scalar,
}

impl Camera {
    /// Convert from metres to the GUI scalar value.
    fn metres_to_scalar(&self, Metres(metres): Metres) -> Scalar {
        self.zoom * metres * self.floorplan_pixels_per_metre
    }

    /// Convert from the GUI scalar value to metres.
    fn scalar_to_metres(&self, scalar: Scalar) -> Metres {
        Metres((scalar / self.zoom) / self.floorplan_pixels_per_metre)
    }
}

struct Log<T> {
    // Newest to oldest is stored front to back respectively.
    deque: VecDeque<T>,
    // The index of the oldest message currently stored in the deque.
    start_index: usize,
    // The max number of messages stored in the log at one time.
    limit: usize,
}

type OscLog = Log<(SocketAddr, OscMessage)>;
type InteractionLog = Log<Interaction>;

impl<T> Log<T> {
    // Construct an OscLog that stores the given max number of messages.
    fn with_limit(limit: usize) -> Self {
        Log {
            deque: VecDeque::new(),
            start_index: 0,
            limit,
        }
    }

    // Push a new OSC message to the log.
    fn push_msg(&mut self, msg: T) {
        self.deque.push_front(msg);
        while self.deque.len() > self.limit {
            self.deque.pop_back();
            self.start_index += 1;
        }
    }
}

impl OscLog {
    // Format the log in a single string of messages.
    fn format(&self) -> String {
        let mut s = String::new();
        let mut index = self.start_index + self.deque.len();
        for &(ref addr, ref msg) in &self.deque {
            let addr_string = format!("{}: [{}{}]\n", index, addr, msg.addr);
            s.push_str(&addr_string);

            // Arguments.
            if let Some(ref args) = msg.args {
                for arg in args {
                    s.push_str(&format!("    {:?}\n", arg));
                }
            }

            index -= 1;
        }
        s
    }
}

impl InteractionLog {
    // Format the log in a single string of messages.
    fn format(&self) -> String {
        let mut s = String::new();
        let mut index = self.start_index + self.deque.len();
        for &interaction in &self.deque {
            let line = format!("{}: {:?}\n", index, interaction);
            s.push_str(&line);
            index -= 1;
        }
        s
    }
}

impl<T> Deref for Log<T> {
    type Target = VecDeque<T>;
    fn deref(&self) -> &Self::Target {
        &self.deque
    }
}

/// The directory in which all fonts are stored.
fn fonts_directory(assets: &Path) -> PathBuf {
    assets.join("fonts")
}

/// The directory in which all images are stored.
fn images_directory(assets: &Path) -> PathBuf {
    assets.join("images")
}

/// Load the image at the given path into a texture.
///
/// Returns the dimensions of the image alongside the texture.
fn load_image(
    path: &Path,
    display: &glium::Display,
) -> ((Scalar, Scalar), glium::texture::Texture2d) {
    let rgba_image = image::open(&path).unwrap().to_rgba();
    let (w, h) = rgba_image.dimensions();
    let raw_image =
        glium::texture::RawImage2d::from_raw_rgba_reversed(&rgba_image.into_raw(), (w, h));
    let texture = glium::texture::Texture2d::new(display, raw_image).unwrap();
    ((w as Scalar, h as Scalar), texture)
}

/// Insert the image at the given path into the given `ImageMap`.
///
/// Return its Id and Dimensions in the form of an `Image`.
fn insert_image(path: &Path, display: &glium::Display, image_map: &mut ImageMap) -> Image {
    let ((width, height), texture) = load_image(path, display);
    let id = image_map.insert(texture);
    let image = Image { id, width, height };
    image
}

/// Spawn the GUI thread.
///
/// The GUI thread is driven by input sent from the main thread. It sends back graphics primitives
/// when a received `Message` would require redrawing the GUI.
pub fn spawn(
    assets: &Path,
    config: Config,
    display: &glium::Display,
    events_loop_proxy: glium::glutin::EventsLoopProxy,
    osc_msg_rx: mpsc::Receiver<(SocketAddr, OscMessage)>,
    interaction_rx: mpsc::Receiver<Interaction>,
    audio_msg_tx: mpsc::Sender<audio::Message>,
) -> (Renderer, ImageMap, mpsc::Sender<Message>, mpsc::Receiver<OwnedPrimitives>) {
    // Use the width and height of the display as the initial size for the Ui.
    let (display_w, display_h) = display.gl_window().get_inner_size_points().unwrap();
    let ui_dimensions = [display_w as Scalar, display_h as Scalar];
    let theme = theme::construct();
    let mut ui = UiBuilder::new(ui_dimensions).theme(theme).build();

    // The type containing the unique ID for each widget in the GUI.
    let mut ids = Ids::new(ui.widget_id_generator());

    // Load and insert the fonts to be used.
    let font_path = fonts_directory(assets).join("NotoSans/NotoSans-Regular.ttf");
    let notosans_regular = ui.fonts.insert_from_file(font_path).unwrap();
    let fonts = Fonts { notosans_regular };

    // Load and insert the images to be used.
    let mut image_map = ImageMap::new();
    let floorplan_path = images_directory(assets).join("floorplan.png");
    let floorplan = insert_image(&floorplan_path, display, &mut image_map);
    let images = Images { floorplan };

    // State that is specific to the GUI itself.
    let mut state = State {
        config,
        // TODO: Possibly load camera from file.
        camera: Camera {
            floorplan_pixels_per_metre: config.floorplan_pixels_per_metre,
            position: cgmath::Point2 { x: Metres(0.0), y: Metres(0.0) },
            zoom: 0.0,
        },
        speaker_editor: SpeakerEditor {
            is_open: true,
            speakers: Vec::new(),
            selected: None,
            next_id: audio::SpeakerId(0),
            audio_msg_tx,
        },
        osc_log: Log::with_limit(config.osc_log_limit),
        interaction_log: Log::with_limit(config.interaction_log_limit),
        side_menu_is_open: true,
        osc_log_is_open: false,
        interaction_log_is_open: false,
    };

    // A renderer from conrod primitives to the OpenGL display.
    let renderer = Renderer::new(display).unwrap();

    // Channels for communication with the main thread.
    let (msg_tx, msg_rx) = mpsc::channel();
    let (render_tx, render_rx) = mpsc::channel();

    // Spawn a thread that converts the OSC messages to GUI messages.
    let msg_tx_clone = msg_tx.clone();
    std::thread::Builder::new()
        .name("osc_to_gui_msg".into())
        .spawn(move || {
            for (addr, msg) in osc_msg_rx {
                if msg_tx_clone.send(Message::Osc(addr, msg)).is_err() {
                    break;
                }
            }
        })
        .unwrap();

    // Spawn a thread that converts the Interaction messages to GUI messages.
    let msg_tx_clone = msg_tx.clone();
    std::thread::Builder::new()
        .name("interaction_to_gui_msg".into())
        .spawn(move || {
            for interaction in interaction_rx {
                if msg_tx_clone.send(Message::Interaction(interaction)).is_err() {
                    break;
                }
            }
        })
        .unwrap();

    // Spawn the main GUI thread.
    std::thread::Builder::new()
        .name("conrod_gui".into())
        .spawn(move || {

            // Many widgets require another frame to finish drawing after clicks or hovers, so we
            // insert an update into the conrod loop using this `bool` after each event.
            let mut needs_update = true;

            // A buffer for collecting OSC messages.
            let mut msgs = Vec::new();

            'conrod: loop {
                // Collect any pending messages.
                msgs.extend(msg_rx.try_iter());

                // If there are no messages pending, wait for them.
                if msgs.is_empty() && !needs_update {
                    match msg_rx.recv() {
                        Ok(msg) => msgs.push(msg),
                        Err(_) => break 'conrod,
                    };
                }

                needs_update = false;
                for msg in msgs.drain(..) {
                    match msg {
                        Message::Osc(addr, osc) =>
                            state.osc_log.push_msg((addr, osc)),
                        Message::Interaction(interaction) =>
                            state.interaction_log.push_msg(interaction),
                        Message::Input(input) => {
                            ui.handle_event(input);
                            needs_update = true;
                        },
                    }
                }

                // Instantiate the widgets.
                {
                    let mut gui = Gui {
                        ui: ui.set_widgets(),
                        ids: &mut ids,
                        images: &images,
                        fonts: &fonts,
                        state: &mut state,
                    };
                    set_widgets(&mut gui);
                }

                // Render the `Ui` to a list of primitives that we can send to the main thread for
                // display. Wakeup `winit` for rendering.
                if let Some(primitives) = ui.draw_if_changed() {
                    if render_tx.send(primitives.owned()).is_err() ||
                        events_loop_proxy.wakeup().is_err()
                    {
                        break 'conrod;
                    }
                }
            }
        })
        .unwrap();

    (renderer, image_map, msg_tx, render_rx)
}

/// Draws the given `primitives` to the given `Display`.
pub fn draw(
    display: &glium::Display,
    renderer: &mut Renderer,
    image_map: &ImageMap,
    primitives: &OwnedPrimitives,
) {
    use conrod::backend::glium::glium::Surface;
    renderer.fill(display, primitives.walk(), &image_map);
    let mut target = display.draw();
    target.clear_color(0.0, 0.0, 0.0, 1.0);
    renderer.draw(display, &mut target, &image_map).unwrap();
    target.finish().unwrap();
}

// A unique ID foor each widget in the GUI.
widget_ids! {
    pub struct Ids {
        // The backdrop for all widgets.
        background,

        // The canvas for the menu to the left of the GUI.
        side_menu,
        // The menu button at the top of the sidebar.
        side_menu_button,
        side_menu_button_line_top,
        side_menu_button_line_middle,
        side_menu_button_line_bottom,
        // OSC Log.
        osc_log,
        osc_log_text,
        osc_log_scrollbar_y,
        osc_log_scrollbar_x,
        // Interaction Log.
        interaction_log,
        interaction_log_text,
        interaction_log_scrollbar_y,
        interaction_log_scrollbar_x,
        // Speaker Editor.
        speaker_editor,
        speaker_editor_no_speakers,
        speaker_editor_list,
        speaker_editor_add,
        speaker_editor_remove,
        speaker_editor_selected_canvas,
        speaker_editor_selected_none,
        speaker_editor_selected_name,
        speaker_editor_selected_channel,
        speaker_editor_selected_position,

        // The floorplan image and the canvas on which it is placed.
        floorplan_canvas,
        floorplan,
        floorplan_speakers[],
    }
}

// Set the widgets in the side menu.
fn set_side_menu_widgets(gui: &mut Gui) {

    const ITEM_HEIGHT: Scalar = 30.0;
    const SMALL_FONT_SIZE: FontSize = 12;

    // Begin building a `CollapsibleArea` for the sidebar.
    fn collapsible_area(is_open: bool, text: &str, side_menu_id: widget::Id)
        -> widget::CollapsibleArea
    {
        widget::CollapsibleArea::new(is_open, text)
            .w_of(side_menu_id)
            .h(ITEM_HEIGHT)
    }

    // Begin building a basic info text block.
    fn info_text(text: &str) -> widget::Text {
        widget::Text::new(&text)
            .font_size(SMALL_FONT_SIZE)
            .line_spacing(6.0)
    }

    // Speaker Editor - for adding, editing and removing speakers.
    let last_area_id = {
        let is_open = gui.state.speaker_editor.is_open;
        const LIST_HEIGHT: Scalar = 140.0;
        const PAD: Scalar = 6.0;
        const TEXT_PAD: Scalar = 20.0;

        const SELECTED_CANVAS_H: Scalar = ITEM_HEIGHT * 2.0 + PAD * 3.0;
        let speaker_editor_canvas_h = LIST_HEIGHT + ITEM_HEIGHT + SELECTED_CANVAS_H;

        let (area, event) = collapsible_area(is_open, "Speaker Editor", gui.ids.side_menu)
            .align_middle_x_of(gui.ids.side_menu)
            .down_from(gui.ids.side_menu_button, 0.0)
            .set(gui.ids.speaker_editor, gui);
        if let Some(event) = event {
            gui.state.speaker_editor.is_open = event.is_open();
        }

        if let Some(area) = area {
            // The canvas on which the log will be placed.
            let canvas = widget::Canvas::new()
                .scroll_kids()
                .pad(0.0)
                .h(speaker_editor_canvas_h);
            area.set(canvas, gui);

            // If there are no speakers, display a message saying how to add some.
            if gui.state.speaker_editor.speakers.is_empty() {
                widget::Text::new("Add some speaker outputs with the `+` button")
                    .padded_w_of(area.id, TEXT_PAD)
                    .mid_top_with_margin_on(area.id, TEXT_PAD)
                    .font_size(SMALL_FONT_SIZE)
                    .center_justify()
                    .set(gui.ids.speaker_editor_no_speakers, gui);

            // Otherwise display the speaker list.
            } else {
                let num_items = gui.state.speaker_editor.speakers.len();
                let (mut events, scrollbar) = widget::ListSelect::single(num_items)
                    .item_size(ITEM_HEIGHT)
                    .h(LIST_HEIGHT)
                    .align_middle_x_of(area.id)
                    .align_top_of(area.id)
                    .scrollbar_next_to()
                    .set(gui.ids.speaker_editor_list, gui);

                // If a speaker was removed, process it after the whole list is instantiated to avoid
                // invalid indices.
                let mut maybe_remove_index = None;

                while let Some(event) = events.next(gui, |i| gui.state.speaker_editor.selected == Some(i)) {
                    use conrod::widget::list_select::Event;
                    match event {

                        // Instantiate a button for each speaker.
                        Event::Item(item) => {
                            let selected = gui.state.speaker_editor.selected == Some(item.i);
                            let label = {
                                let speaker = &gui.state.speaker_editor.speakers[item.i];
                                let channel = speaker.audio.channel.load(atomic::Ordering::Relaxed);
                                let position = speaker.audio.point.load(atomic::Ordering::Relaxed);
                                let label = format!("{} - CH {} - ({}mx, {}my)",
                                                    speaker.name, channel,
                                                    (position.x.0 * 100.0).trunc() / 100.0,
                                                    (position.y.0 * 100.0).trunc() / 100.0);
                                label
                            };

                            // Blue if selected, gray otherwise.
                            let color = if selected { color::BLUE } else { color::CHARCOAL };

                            // Use `Button`s for the selectable items.
                            let button = widget::Button::new()
                                .label(&label)
                                .label_font_size(SMALL_FONT_SIZE)
                                .color(color);
                            item.set(button, gui);

                            // If the button or any of its children are capturing the mouse, display
                            // the `remove` button.
                            let show_remove_button = gui.global_input().current.widget_capturing_mouse
                                .map(|id| {
                                    id == item.widget_id ||
                                    gui.widget_graph()
                                        .does_recursive_depth_edge_exist(item.widget_id, id)
                                })
                                .unwrap_or(false);

                            if !show_remove_button {
                                continue;
                            }

                            if widget::Button::new()
                                .label("X")
                                .label_font_size(SMALL_FONT_SIZE)
                                .color(color::DARK_RED.alpha(0.5))
                                .w_h(ITEM_HEIGHT, ITEM_HEIGHT)
                                .align_right_of(item.widget_id)
                                .align_middle_y_of(item.widget_id)
                                .parent(item.widget_id)
                                .set(gui.ids.speaker_editor_remove, gui)
                                .was_clicked()
                            {
                                maybe_remove_index = Some(item.i);
                                if selected {
                                    gui.state.speaker_editor.selected = None;
                                }
                            }
                        },

                        // Update the selected speaker.
                        Event::Selection(idx) => gui.state.speaker_editor.selected = Some(idx),

                        _ => (),
                    }
                }

                // The scrollbar for the list.
                if let Some(s) = scrollbar { s.set(gui); }

                // Remove a speaker if necessary.
                if let Some(i) = maybe_remove_index {
                    let speaker = gui.state.speaker_editor.speakers.remove(i);
                    let msg = audio::Message::RemoveSpeaker(speaker.id);
                    gui.state.speaker_editor.audio_msg_tx
                        .send(msg)
                        .expect("audio_mst_tx was closed");
                }
            }

            // Only display the `add_speaker` button if there are less than `max` num channels.
            let show_add_button = gui.state.speaker_editor.speakers.len() < audio::MAX_CHANNELS;

            if show_add_button {
                let plus_size = (ITEM_HEIGHT * 0.66) as FontSize;
                if widget::Button::new()
                    .color(color::rgb(0.1, 0.13, 0.15))
                    .label("+")
                    .label_font_size(plus_size)
                    .align_middle_x_of(area.id)
                    .mid_top_with_margin_on(area.id, LIST_HEIGHT)
                    .w_of(area.id)
                    .parent(area.id)
                    .set(gui.ids.speaker_editor_add, gui)
                    .was_clicked()
                {
                    let id = gui.state.speaker_editor.next_id;
                    let name = format!("S{}", id.0);
                    let channel = {
                        // Search for the next available channel starting from 0.
                        //
                        // Note: This is a super naiive way of searching however there should never
                        // be enough speakers to make it a problem.
                        let mut channel = 0;
                        'search: loop {
                            for speaker in &gui.state.speaker_editor.speakers {
                                if channel == speaker.audio.channel.load(atomic::Ordering::Relaxed) {
                                    channel += 1;
                                    continue 'search;
                                }
                            }
                            break channel;
                        }
                    };
                    let audio = Arc::new(audio::Speaker {
                        point: Atomic::new(gui.state.camera.position),
                        channel: AtomicUsize::new(channel),
                    });
                    let speaker = Speaker { id, name, audio };

                    gui.state.speaker_editor.audio_msg_tx
                        .send(audio::Message::AddSpeaker(speaker.id, speaker.audio.clone()))
                        .expect("audio_msg_tx was closed");
                    gui.state.speaker_editor.speakers.push(speaker);
                    gui.state.speaker_editor.next_id = audio::SpeakerId(id.0.wrapping_add(1));
                    gui.state.speaker_editor.selected = Some(gui.state.speaker_editor.speakers.len() - 1);
                }
            }

            let area_rect = gui.rect_of(area.id).unwrap();
            let start = area_rect.y.start;
            let end = start + SELECTED_CANVAS_H;
            let selected_canvas_y = conrod::Range { start, end };

            widget::Canvas::new()
                .pad(PAD)
                .w_of(gui.ids.side_menu)
                .h(SELECTED_CANVAS_H)
                .y(selected_canvas_y.middle())
                .align_middle_x_of(gui.ids.side_menu)
                .set(gui.ids.speaker_editor_selected_canvas, gui);


            // If a speaker is selected, display its info.
            if let Some(i) = gui.state.speaker_editor.selected {
                let Gui { ref mut state, ref mut ui, ref ids, .. } = *gui;
                let speakers = &mut state.speaker_editor.speakers;

                for event in widget::TextBox::new(&speakers[i].name)
                    .mid_top_of(ids.speaker_editor_selected_canvas)
                    .kid_area_w_of(ids.speaker_editor_selected_canvas)
                    .parent(gui.ids.speaker_editor_selected_canvas)
                    .h(ITEM_HEIGHT)
                    .set(ids.speaker_editor_selected_name, ui)
                {
                    if let widget::text_box::Event::Update(string) = event {
                        speakers[i].name = string;
                    }
                }

                let channels: Vec<String> = (0..audio::MAX_CHANNELS)
                    .map(|ch| {
                        speakers
                            .iter()
                            .enumerate()
                            .find(|&(ix, s)| i != ix && s.audio.channel.load(atomic::Ordering::Relaxed) == ch)
                            .map(|(_ix, s)| format!("CH {} (swap with {})", ch, &s.name))
                            .unwrap_or_else(|| format!("CH {}", ch))
                    })
                    .collect();
                let selected = speakers[i].audio.channel.load(atomic::Ordering::Relaxed);

                for index in widget::DropDownList::new(&channels, Some(selected))
                    .down_from(ids.speaker_editor_selected_name, PAD)
                    .align_middle_x_of(ids.side_menu)
                    .kid_area_w_of(ids.speaker_editor_selected_canvas)
                    .h(ITEM_HEIGHT)
                    .parent(ids.speaker_editor_selected_canvas)
                    .scrollbar_on_top()
                    .max_visible_items(5)
                    .border_color(color::LIGHT_CHARCOAL)
                    .set(ids.speaker_editor_selected_channel, ui)
                {
                    speakers[i].audio.channel.store(index, atomic::Ordering::Relaxed);
                    // If an existing speaker was assigned to `index`, swap it with the original
                    // selection.
                    let maybe_index = speakers.iter()
                        .enumerate()
                        .find(|&(ix, s)| i != ix && s.audio.channel.load(atomic::Ordering::Relaxed) == index);
                    if let Some((ix, _)) = maybe_index {
                        speakers[ix].audio.channel.store(selected, atomic::Ordering::Relaxed);
                    }
                }

            // Otherwise no speaker is selected.
            } else {
                widget::Text::new("No speaker selected")
                    .padded_w_of(area.id, TEXT_PAD)
                    .mid_top_with_margin_on(gui.ids.speaker_editor_selected_canvas, TEXT_PAD)
                    .font_size(SMALL_FONT_SIZE)
                    .center_justify()
                    .set(gui.ids.speaker_editor_selected_none, gui);
            }

            area.id
        } else {
            gui.ids.speaker_editor
        }
    };

    // The log of received OSC messages.
    let last_area_id = {
        let is_open = gui.state.osc_log_is_open;
        let log_canvas_h = 200.0;
        let (area, event) = collapsible_area(is_open, "OSC Input Log", gui.ids.side_menu)
            .align_middle_x_of(gui.ids.side_menu)
            .down_from(last_area_id, 0.0)
            .set(gui.ids.osc_log, gui);
        if let Some(event) = event {
            gui.state.osc_log_is_open = event.is_open();
        }
        if let Some(area) = area {

            // The canvas on which the log will be placed.
            let canvas = widget::Canvas::new()
                .scroll_kids()
                .pad(10.0)
                .h(log_canvas_h);
            area.set(canvas, gui);

            // The text widget used to display the log.
            let log_string = match gui.state.osc_log.len() {
                0 => format!("No messages received yet.\nListening on port {}...",
                             gui.state.config.osc_input_port),
                _ => gui.state.osc_log.format(),
            };
            info_text(&log_string)
                .top_left_of(area.id)
                .kid_area_w_of(area.id)
                .set(gui.ids.osc_log_text, gui);

            // Scrollbars.
            widget::Scrollbar::y_axis(area.id)
                .color(color::LIGHT_CHARCOAL)
                .auto_hide(false)
                .set(gui.ids.osc_log_scrollbar_y, gui);
            widget::Scrollbar::x_axis(area.id)
                .color(color::LIGHT_CHARCOAL)
                .auto_hide(true)
                .set(gui.ids.osc_log_scrollbar_x, gui);

            area.id
        } else {
            gui.ids.osc_log
        }
    };

    // The log of received Interactions.
    let last_area_id = {
        let is_open = gui.state.interaction_log_is_open;
        let log_canvas_h = 200.0;
        let (area, event) = collapsible_area(is_open, "Interaction Log", gui.ids.side_menu)
            .align_middle_x_of(gui.ids.side_menu)
            .down_from(last_area_id, 0.0)
            .set(gui.ids.interaction_log, gui);
        if let Some(event) = event {
            gui.state.interaction_log_is_open = event.is_open();
        }

        if let Some(area) = area {
            // The canvas on which the log will be placed.
            let canvas = widget::Canvas::new()
                .scroll_kids()
                .pad(10.0)
                .h(log_canvas_h);
            area.set(canvas, gui);

            // The text widget used to display the log.
            let log_string = match gui.state.interaction_log.len() {
                0 => format!("No interactions received yet.\nListening on port {}...",
                             gui.state.config.osc_input_port),
                _ => gui.state.interaction_log.format(),
            };
            info_text(&log_string)
                .top_left_of(area.id)
                .kid_area_w_of(area.id)
                .set(gui.ids.interaction_log_text, gui);

            // Scrollbars.
            widget::Scrollbar::y_axis(area.id)
                .color(color::LIGHT_CHARCOAL)
                .auto_hide(false)
                .set(gui.ids.interaction_log_scrollbar_y, gui);
            widget::Scrollbar::x_axis(area.id)
                .color(color::LIGHT_CHARCOAL)
                .auto_hide(true)
                .set(gui.ids.interaction_log_scrollbar_x, gui);

            area.id
        } else {
            gui.ids.interaction_log
        }
    };

}

// Update all widgets in the GUI with the given state.
fn set_widgets(gui: &mut Gui) {

    let background_color = color::WHITE;

    // The background for the main `UI` window.
    widget::Canvas::new()
        .color(background_color)
        .pad(0.0)
        .parent(gui.window)
        .middle_of(gui.window)
        .wh_of(gui.window)
        .set(gui.ids.background, gui);

    // A thin menu bar on the left.
    //
    // The menu bar is collapsed by default, and shows three lines at the top.
    // Pressing these three lines opens the menu, revealing a list of options.
    const CLOSED_SIDE_MENU_W: conrod::Scalar = 40.0;
    const OPEN_SIDE_MENU_W: conrod::Scalar = 300.0;
    let side_menu_is_open = gui.state.side_menu_is_open;
    let side_menu_w = match side_menu_is_open {
        false => CLOSED_SIDE_MENU_W,
        true => OPEN_SIDE_MENU_W,
    };

    // The canvas on which all side_menu widgets are placed.
    widget::Canvas::new()
        .w(side_menu_w)
        .h_of(gui.ids.background)
        .mid_left_of(gui.ids.background)
        .pad(0.0)
        .color(color::rgb(0.1, 0.13, 0.15))
        .set(gui.ids.side_menu, gui);

    // The classic three line menu button for opening the side_menu.
    for _click in widget::Button::new()
        .w_h(side_menu_w, CLOSED_SIDE_MENU_W)
        .mid_top_of(gui.ids.side_menu)
        //.color(color::BLACK)
        .color(color::rgb(0.07, 0.08, 0.09))
        .set(gui.ids.side_menu_button, gui)
    {
        gui.state.side_menu_is_open = !side_menu_is_open;
    }

    // Draw the three lines using rectangles.
    fn menu_button_line(menu_button: widget::Id) -> widget::Rectangle {
        let line_h = 2.0;
        let line_w = CLOSED_SIDE_MENU_W / 3.0;
        widget::Rectangle::fill([line_w, line_h])
            .color(color::WHITE)
            .graphics_for(menu_button)
    }

    let margin = CLOSED_SIDE_MENU_W / 3.0;
    menu_button_line(gui.ids.side_menu_button)
        .mid_top_with_margin_on(gui.ids.side_menu_button, margin)
        .set(gui.ids.side_menu_button_line_top, gui);
    menu_button_line(gui.ids.side_menu_button)
        .middle_of(gui.ids.side_menu_button)
        .set(gui.ids.side_menu_button_line_middle, gui);
    menu_button_line(gui.ids.side_menu_button)
        .mid_bottom_with_margin_on(gui.ids.side_menu_button, margin)
        .set(gui.ids.side_menu_button_line_bottom, gui);

    // If the side_menu is open, set all the side_menu widgets.
    if side_menu_is_open {
        set_side_menu_widgets(gui);
    }

    // The canvas on which the floorplan will be displayed.
    let background_rect = gui.rect_of(gui.ids.background).unwrap();
    let floorplan_canvas_w = background_rect.w() - side_menu_w;
    let floorplan_canvas_h = background_rect.h();
    widget::Canvas::new()
        .w_h(floorplan_canvas_w, floorplan_canvas_h)
        .h_of(gui.ids.background)
        .color(color::WHITE)
        .align_right_of(gui.ids.background)
        .align_middle_y_of(gui.ids.background)
        .crop_kids()
        .set(gui.ids.floorplan_canvas, gui);

    let floorplan_pixels_per_metre = gui.state.camera.floorplan_pixels_per_metre;
    let metres_from_floorplan_pixels = |px| Metres(px / floorplan_pixels_per_metre);
    let metres_to_floorplan_pixels = |Metres(m)| m * floorplan_pixels_per_metre;

    let floorplan_w_metres = metres_from_floorplan_pixels(gui.images.floorplan.width);
    let floorplan_h_metres = metres_from_floorplan_pixels(gui.images.floorplan.height);

    // The amount which the image must be scaled to fill the floorplan_canvas while preserving
    // aspect ratio.
    let full_scale_w = floorplan_canvas_w / gui.images.floorplan.width;
    let full_scale_h = floorplan_canvas_h / gui.images.floorplan.height;
    let floorplan_w = full_scale_w * gui.images.floorplan.width;
    let floorplan_h = full_scale_h * gui.images.floorplan.height;

    // If the floorplan was scrolled, adjust the camera zoom.
    let total_scroll = gui.widget_input(gui.ids.floorplan)
        .scrolls()
        .fold(0.0, |acc, scroll| acc + scroll.y);
    gui.state.camera.zoom = (gui.state.camera.zoom - total_scroll / 200.0)
        .max(full_scale_w.min(full_scale_h))
        .min(1.0);

    // Move the camera by clicking with the left mouse button and dragging.
    let total_drag = gui.widget_input(gui.ids.floorplan)
        .drags()
        .left()
        .map(|drag| drag.delta_xy)
        .fold([0.0, 0.0], |acc, dt| [acc[0] + dt[0], acc[1] + dt[1]]);
    gui.state.camera.position.x -= gui.state.camera.scalar_to_metres(total_drag[0]);
    gui.state.camera.position.y -= gui.state.camera.scalar_to_metres(total_drag[1]);

    // The part of the image visible from the camera.
    let visible_w_m = gui.state.camera.scalar_to_metres(floorplan_canvas_w);
    let visible_h_m = gui.state.camera.scalar_to_metres(floorplan_canvas_h);

    // Clamp the camera's position so it doesn't go out of bounds.
    let invisible_w_m = floorplan_w_metres - visible_w_m;
    let invisible_h_m = floorplan_h_metres - visible_h_m;
    let half_invisible_w_m = invisible_w_m * 0.5;
    let half_invisible_h_m = invisible_h_m * 0.5;
    let centre_x_m = floorplan_w_metres * 0.5;
    let centre_y_m = floorplan_h_metres * 0.5;
    let min_cam_x_m = centre_x_m - half_invisible_w_m;
    let max_cam_x_m = centre_x_m + half_invisible_w_m;
    let min_cam_y_m = centre_y_m - half_invisible_h_m;
    let max_cam_y_m = centre_y_m + half_invisible_h_m;
    gui.state.camera.position.x = gui.state.camera.position.x.max(min_cam_x_m).min(max_cam_x_m);
    gui.state.camera.position.y = gui.state.camera.position.y.max(min_cam_y_m).min(max_cam_y_m);

    let visible_x = metres_to_floorplan_pixels(gui.state.camera.position.x);
    let visible_y = metres_to_floorplan_pixels(gui.state.camera.position.y);
    let visible_w = metres_to_floorplan_pixels(visible_w_m);
    let visible_h = metres_to_floorplan_pixels(visible_h_m);
    let visible_rect = conrod::Rect::from_xy_dim([visible_x, visible_y], [visible_w, visible_h]);

    // If the left mouse button was clicked on the floorplan, deselect the speaker.
    if gui.widget_input(gui.ids.floorplan).clicks().left().next().is_some() {
        gui.state.speaker_editor.selected = None;
    }

    // Display the floorplan.
    widget::Image::new(gui.images.floorplan.id)
        .source_rectangle(visible_rect)
        .w_h(floorplan_w, floorplan_h)
        .middle_of(gui.ids.floorplan_canvas)
        .set(gui.ids.floorplan, gui);

    // Draw the speakers over the floorplan.
    {
        let Gui { ref mut ids, ref mut state, ref mut ui, .. } = *gui;

        // Ensure there are enough IDs available.
        let num_speakers = state.speaker_editor.speakers.len();
        if ids.floorplan_speakers.len() < num_speakers {
            let id_gen = &mut ui.widget_id_generator();
            ids.floorplan_speakers.resize(num_speakers, id_gen);
        }

        // Display the `gui.state.speaker_editor.speakers` over the floorplan as circles.
        let radius_min_m = state.config.min_speaker_radius_metres;
        let radius_max_m = state.config.max_speaker_radius_metres;
        let radius_min = state.camera.metres_to_scalar(radius_min_m);
        let radius_max = state.camera.metres_to_scalar(radius_max_m);

        let rel_point_to_metres = |cam: &Camera, p: conrod::Point| -> cgmath::Point2<Metres> {
            let x = cam.position.x + cam.scalar_to_metres(p[0]);
            let y = cam.position.y + cam.scalar_to_metres(p[1]);
            cgmath::Point2 { x, y }
        };

        for i in 0..state.speaker_editor.speakers.len() {
            let widget_id = ids.floorplan_speakers[i];

            let (dragged_x, dragged_y) = ui.widget_input(widget_id)
                .drags()
                .left()
                .fold((0.0, 0.0), |(x, y), drag| (x + drag.delta_xy[0], y + drag.delta_xy[1]));
            let dragged_x_m = state.camera.scalar_to_metres(dragged_x);
            let dragged_y_m = state.camera.scalar_to_metres(dragged_y);

            let position = {
                let p = state.speaker_editor.speakers[i].audio.point.load(atomic::Ordering::Relaxed);
                let x = p.x + dragged_x_m;
                let y = p.y + dragged_y_m;
                let new_p = cgmath::Point2 { x, y };
                if p != new_p {
                    state.speaker_editor.speakers[i].audio.point.store(new_p, atomic::Ordering::Relaxed);
                }
                new_p
            };

            let x = state.camera.metres_to_scalar(position.x - state.camera.position.x);
            let y = state.camera.metres_to_scalar(position.y - state.camera.position.y);

            // Select the speaker if it was pressed.
            if ui.widget_input(widget_id)
                .presses()
                .mouse()
                .left()
                .next()
                .is_some()
            {
                state.speaker_editor.selected = Some(i);
            }

            // Give some tactile colour feedback if the speaker is interacted with.
            let color = if Some(i) == state.speaker_editor.selected { color::BLUE } else { color::DARK_RED };
            let color = match ui.widget_input(widget_id).mouse() {
                Some(mouse) =>
                    if mouse.buttons.left().is_down() { color.clicked() }
                    else { color.highlighted() },
                None => color,
            };

            // Display a circle for the speaker.
            widget::Circle::fill(radius_min)
                .x_y_relative_to(ids.floorplan, x, y)
                .parent(ids.floorplan)
                .color(color)
                .set(widget_id, ui);
        }
    }

}