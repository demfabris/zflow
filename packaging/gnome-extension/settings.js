import Adw from 'gi://Adw?version=1';
import Gdk from 'gi://Gdk?version=4.0';
import Gtk from 'gi://Gtk?version=4.0';
import Pango from 'gi://Pango';
import {Client, compatible, statusText} from './client.js';

// src/core/keymap.rs KeyboardMode, in dropdown order.
const KEYBOARD_MODES = ['standard', 'pc_positions', 'mac'];
const HEALTH_ICONS = {ok: 'object-select-symbolic', warning: 'dialog-warning-symbolic', error: 'dialog-error-symbolic'};
const NO_LAYOUT = 'Arrange computers in zflow on the other computer.';
const LAYOUT_HINT = 'Arrange individual monitors. Use exposed edges to switch computers; system display settings control movement between local monitors.';
// As in the Mac app (ComputerLayout.swift): an arrow key moves a tile 100
// layout units and snaps within 150; a drag snaps within 14 pixels.
const KEY_STEP = 100;
const KEY_TOLERANCE = 150;
const DRAG_TOLERANCE = 14;
const ARROWS = {[Gdk.KEY_Left]: [-1, 0], [Gdk.KEY_Right]: [1, 0], [Gdk.KEY_Up]: [0, -1], [Gdk.KEY_Down]: [0, 1]};
const BOARD_HEIGHT = 200;
// The shelf of computers found on the network, under the board.
const FOUND_SIZE = [132, 64];
const SHELF_TOP = 48;
const SHELF_GAP = 8;
// The size the service gives a placed computer's tile (PEER_TILE_SIZE in
// src/daemon.rs) until that computer sends its own.
const NEW_TILE = [1920, 1080];
const PLACEABLE = new Set(['ready', 'duplicate_name']);
const OS_NAMES = {linux: 'Linux', macos: 'macOS', windows: 'Windows'};
// A key's mark is six hex digits; each of the first four picks one of these
// by its low three bits, as the Mac app does, so a key looks the same on
// every computer.
const MARK_COLORS = ['#E5484D', '#F76B15', '#FFC53D', '#30A46C', '#12A594', '#0090FF', '#8E4EC6', '#D6409F'];
const CSS = `
.zf-shelf { border: 1.5px dashed alpha(currentColor, 0.25); border-radius: 12px; padding: 8px 12px; }
.zf-mark { min-width: 7px; min-height: 7px; border-radius: 2px; }
${MARK_COLORS.map((color, index) => `.zf-mark-${index} { background: ${color}; }`).join('\n')}
`;

function button(label, action, css = []) {
    const widget = new Gtk.Button({label, valign: Gtk.Align.CENTER, css_classes: css});
    widget.connect('clicked', action);
    return widget;
}

let styled = false;
function addStyle() {
    if (styled) return;
    styled = true;
    const provider = new Gtk.CssProvider();
    provider.load_from_string(CSS);
    Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION);
}

// The palette indexes of a mark's four squares, from its six hex digits.
export function markColors(seed) {
    if (!/^[0-9a-f]{6}$/i.test(seed ?? '')) return [];
    return [...seed.slice(0, 4)].map(digit => Number.parseInt(digit, 16) & 7);
}

// Draws a mark into `box` as four small squares, or nothing without one.
function showMark(box, seed) {
    if (box._seed === seed) return;
    box._seed = seed;
    for (let child = box.get_first_child(); child; child = box.get_first_child()) box.remove(child);
    for (const color of markColors(seed)) box.append(new Gtk.Box({css_classes: ['zf-mark', `zf-mark-${color}`], valign: Gtk.Align.CENTER}));
    box.visible = !!box.get_first_child();
    // The text form a notification or `zflow nearby` gives.
    box.tooltip_text = box.visible ? `Mark ${seed}` : null;
}

export class Settings {
    constructor(window) {
        this.window = window;
        this._updating = false;
        this._busy = false;
        this._disposed = false;
        this._peerKey = '';
        this._peerRows = [];
        this._healthKey = '';
        this._shortcutKey = '';
        this._keyboards = new Map();
        this._controls = new Map();
        this._scrolls = new Map();
        // Rows are rebuilt when a computer connects, so remember which are open.
        this._expanded = new Set();
        this.page = new Adw.PreferencesPage({title: 'zflow', icon_name: 'io.zflow.zflow-symbolic'});
        const sharing = new Adw.PreferencesGroup();
        this._status = new Adw.ActionRow({title: 'Starting zflow…', subtitle: 'Share your keyboard, pointer, and trackpad.', subtitle_lines: 3, use_markup: false});
        this._statusIcon = new Gtk.Image({icon_name: 'input-mouse-symbolic'});
        this._status.add_prefix(this._statusIcon);
        sharing.add(this._status);
        this._sharing = new Adw.SwitchRow({title: 'Input Sharing', subtitle: 'Allow input to move between paired computers.', sensitive: false});
        this._sharing.connect('notify::active', () => {
            if (!this._updating) this._run({command: 'set_sharing', enabled: this._sharing.active});
        });
        sharing.add(this._sharing);
        this.page.add(sharing);
        this._health = new Adw.PreferencesGroup({title: 'Checks', visible: false});
        this.page.add(this._health);
        // Requests that failed; checks that fail are in the group above.
        this._errorGroup = new Adw.PreferencesGroup({visible: false});
        this._error = new Adw.ActionRow({title: 'Needs attention', subtitle_lines: 0});
        this._error.add_prefix(new Gtk.Image({icon_name: 'dialog-warning-symbolic'}));
        this._errorGroup.add(this._error);
        this.page.add(this._errorGroup);

        addStyle();
        this._computers = new Adw.PreferencesGroup({title: 'Computers', description: NO_LAYOUT});
        this._addButton = button('Add by Address…', () => this._openAddAddress(), ['flat']);
        this._computers.header_suffix = this._addButton;
        // A fresh install's pairing window, and the computer it took.
        this._windowRow = new Adw.ActionRow({title: '', subtitle_lines: 0, use_markup: false, visible: false});
        this._windowMark = new Gtk.Box({spacing: 2});
        this._windowRow.add_prefix(this._windowMark);
        this._computers.add(this._windowRow);
        this._dismissed = 0;
        this._joinedRow = new Adw.ActionRow({title: '', subtitle: 'It can share this computer’s keyboard and mouse. Not yours?', subtitle_lines: 0, use_markup: false, visible: false});
        this._joinedMark = new Gtk.Box({spacing: 2});
        this._joinedRow.add_prefix(this._joinedMark);
        this._joinedRow.add_suffix(button('Forget', () => this._forgetJoined(), ['destructive-action']));
        const dismiss = new Gtk.Button({icon_name: 'window-close-symbolic', tooltip_text: 'Dismiss', valign: Gtk.Align.CENTER, css_classes: ['flat']});
        dismiss.connect('clicked', () => this._dismissJoined());
        this._joinedRow.add_suffix(dismiss);
        this._computers.add(this._joinedRow);
        // The layout comes first, above the computers' rows. Each tile is a
        // button, so it takes keyboard focus and has a name. Computers found
        // on the network wait on a shelf under it until one is dragged in.
        this._tiles = new Map();
        this._found = new Map();
        this._layoutKey = '';
        this._foundKey = '';
        this._pressed = null;
        this._boardSize = [440, BOARD_HEIGHT];
        this._shelfHeight = 0;
        this._board = new Gtk.Fixed();
        this._shelf = new Gtk.Box({orientation: Gtk.Orientation.VERTICAL, spacing: 2, css_classes: ['zf-shelf'], visible: false});
        this._shelf.append(new Gtk.Label({label: 'Found on your network', xalign: 0, css_classes: ['heading']}));
        this._shelf.append(new Gtk.Label({label: 'Drag one next to a screen to add it.', xalign: 0, css_classes: ['caption', 'dim-label']}));
        this._board.put(this._shelf, 0, 0);
        // One drag for the whole box. On a tile, it would measure from the
        // tile that moves under it. It sees a press before the tile's button
        // does, as a scrolled window's drag does, and takes it only once the
        // pointer moves.
        const drag = new Gtk.GestureDrag({propagation_phase: Gtk.PropagationPhase.CAPTURE});
        drag.connect('drag-begin', (_drag, x, y) => {
            const under = tile => x >= tile.x && y >= tile.y && x < tile.x + tile.size[0] && y < tile.y + tile.size[1];
            // Later tiles draw on top.
            this._pressed = [...this._tiles.values()].findLast(under)
                ?? [...this._found.values()].find(tile => under(tile) && PLACEABLE.has(tile.found.state));
        });
        drag.connect('drag-update', (_drag, dx, dy) => this._dragTile(drag, dx, dy));
        drag.connect('drag-end', (_drag, dx, dy) => this._dropTile(dx, dy));
        this._board.add_controller(drag);
        // A Gtk.Fixed does not report its width, so the empty area under it does.
        this._area = new Gtk.DrawingArea({content_height: BOARD_HEIGHT, hexpand: true});
        this._area.connect('resize', (_area, width) => {
            this._boardSize = [width, BOARD_HEIGHT];
            this._placeTiles();
            this._placeShelf();
        });
        const canvas = new Gtk.Overlay({child: this._area, overflow: Gtk.Overflow.HIDDEN});
        canvas.add_overlay(this._board);
        this._layout = new Adw.PreferencesRow({title: 'Layout', child: canvas, activatable: false, focusable: false, visible: false});
        this._computers.add(this._layout);
        this.page.add(this._computers);
        const switching = new Adw.PreferencesGroup();
        this._pause = new Adw.SwitchRow({title: 'Pause at Edges', subtitle: 'Rest the pointer against an edge for a moment before it crosses.', sensitive: false});
        this._pause.connect('notify::active', () => {
            if (!this._updating) this._run({command: 'set_switching', pause_at_edges: this._pause.active});
        });
        switching.add(this._pause);
        this.page.add(switching);
        this._shortcuts = new Adw.PreferencesGroup({title: 'Shortcuts', visible: false});
        this.page.add(this._shortcuts);
        const clipboard = new Adw.PreferencesGroup();
        this._clipboard = new Adw.SwitchRow({title: 'Share Clipboard', subtitle: 'Text and images go with the pointer to the other computer. Never files.', sensitive: false});
        this._clipboard.connect('notify::active', () => {
            if (!this._updating) this._run({command: 'set_clipboard', share: this._clipboard.active});
        });
        clipboard.add(this._clipboard);
        this.page.add(clipboard);
        const preferences = new Adw.PreferencesGroup();
        this._login = new Adw.SwitchRow({title: 'Start at Login', subtitle: 'Keep zflow available after you close settings.'});
        this._login.connect('notify::active', () => {
            if (!this._updating) this._run({command: 'set_autostart', enabled: this._login.active});
        });
        preferences.add(this._login);
        this.page.add(preferences);
        // Rows only this computer has, under the shared ones.
        const local = new Adw.PreferencesGroup({title: 'This Computer'});
        const logs = new Adw.ActionRow({title: 'Service Log', subtitle: 'Follow what zflow’s background service is doing.'});
        this._logs = button('Open', () => this._run({command: 'open_logs'}));
        logs.add_suffix(this._logs);
        local.add(logs);
        this.page.add(local);
        this._help = new Adw.PreferencesGroup();
        this.page.add(this._help);
        this.client = new Client(snapshot => this._update(snapshot), true);
        this.client.start();
    }

    _showError(message) {
        if (this._disposed) return;
        this._actionError = message;
        this._error.subtitle = message ?? '';
        this._errorGroup.visible = !!message;
    }

    async _run(request) {
        if (this._busy) return false;
        this._busy = true;
        this._sharing.sensitive = this._login.sensitive = this._addButton.sensitive = this._pause.sensitive = this._clipboard.sensitive = false;
        for (const row of [...this._keyboards.values(), ...this._controls.values(), ...this._scrolls.values()]) row.sensitive = false;
        this._showError(null);
        let success = false;
        try { await this.client.call(request); success = true; }
        catch (error) { this._showError(error.message); }
        finally {
            this._busy = false;
            if (!this._disposed) await this.client.refresh();
        }
        return success;
    }

    _update(snapshot) {
        if (this._disposed) return;
        // An agent from another API level sends a snapshot this window cannot read.
        const known = compatible(snapshot);
        const online = known && snapshot.sharing !== null;
        const state = known ? snapshot.status.state : 'attention';
        this._updating = true;
        this._status.title = statusText(snapshot);
        this._statusIcon.icon_name = state === 'attention' ? 'dialog-warning-symbolic'
            : state === 'paused' ? 'media-playback-pause-symbolic' : 'input-mouse-symbolic';
        this._sharing.active = online && snapshot.sharing;
        this._sharing.sensitive = online && !this._busy;
        this._pause.active = (known && snapshot.pause_at_edges) ?? false;
        this._pause.sensitive = online && snapshot.pause_at_edges !== null && !this._busy;
        // An agent from before clipboard sharing leaves it out.
        const share = known ? (snapshot.share_clipboard ?? null) : null;
        this._clipboard.active = share ?? false;
        this._clipboard.sensitive = online && share !== null && !this._busy;
        this._login.active = (known && snapshot.autostart) ?? false;
        this._login.sensitive = known && snapshot.autostart !== null && !this._busy;
        this._addButton.sensitive = online && !this._busy;
        this._updating = false;
        this._error.subtitle = this._actionError || (known ? '' : snapshot?.error ?? 'Update zflow so this window and the service match.');
        this._errorGroup.visible = !!this._error.subtitle;
        this._help.description = `Changes save automatically. Advanced settings are in ${known ? snapshot.config_path : '/etc/zflow/zflow.toml'}.`;
        this._updateHealth(known ? snapshot.health : []);
        const peers = known ? snapshot.peers : [];
        // An agent from before the shelf leaves these out.
        this._updateLayout(known ? snapshot.layout : null, known ? snapshot.own_mark ?? null : null, peers);
        this._updateFound(known ? snapshot.unplaced ?? [] : []);
        this._updateWindow(known ? snapshot.pairing_window ?? null : null);
        this._updateJoined(known ? snapshot.notices ?? [] : [], peers);
        this._updatePeers(peers);
        this._updateShortcuts(known ? snapshot.shortcuts : []);
    }

    _updateWindow(window) {
        const open = window?.state === 'open';
        this._windowRow.visible = open;
        if (!open) return;
        const holding = window.holding;
        if (holding) {
            this._windowRow.title = `Adding ${holding.name}`;
            this._windowRow.subtitle = `In ${Math.ceil(holding.ms_left / 1000)} s, unless another computer shows up.`;
        } else {
            const minutes = Math.max(1, Math.ceil((window.seconds_left ?? 0) / 60));
            this._windowRow.title = 'This computer is new';
            this._windowRow.subtitle = `For the next ${minutes} ${minutes === 1 ? 'minute' : 'minutes'}, a new computer that shows up alone joins by itself. Or drag one into place below.`;
        }
        showMark(this._windowMark, holding?.mark ?? null);
    }

    // The newest computer that joined, while it is still here, until dismissed.
    _updateJoined(notices, peers) {
        const names = new Set(peers.map(({name}) => name));
        const joined = notices.findLast(notice => notice.kind === 'joined' && notice.id > this._dismissed && names.has(notice.name));
        this._joined = joined ?? null;
        this._joinedRow.visible = !!joined;
        if (joined) this._joinedRow.title = `${joined.name} joined`;
        showMark(this._joinedMark, joined?.mark ?? null);
    }

    _dismissJoined() {
        if (this._joined) this._dismissed = this._joined.id;
        this._joinedRow.visible = false;
    }

    async _forgetJoined() {
        const joined = this._joined;
        if (!joined) return;
        this._dismissJoined();
        await this._run({command: 'forget', name: joined.name});
    }

    _updateHealth(rows) {
        const key = JSON.stringify(rows);
        if (key === this._healthKey) return;
        this._healthKey = key;
        for (const row of this._healthRows ?? []) this._health.remove(row);
        this._healthRows = rows.map(({level, title, detail, action}) => {
            const row = new Adw.ActionRow({title, subtitle: detail, subtitle_lines: 0, use_markup: false});
            row.add_prefix(new Gtk.Image({icon_name: HEALTH_ICONS[level] ?? HEALTH_ICONS.warning}));
            if (action) row.add_suffix(button(action.label, () => this._run({command: action.command})));
            this._health.add(row);
            return row;
        });
        this._health.visible = rows.length > 0;
    }

    _updateLayout(layout, ownMark, peers) {
        const monitors = (layout?.monitors ?? []).filter(m => m.display?.active !== false);
        this._layout.visible = monitors.length > 0;
        this._computers.description = monitors.length ? LAYOUT_HINT : NO_LAYOUT;
        // Each tile shows its computer's key mark.
        const marks = new Map(peers.map(({name, mark}) => [name, mark]));
        const mark = monitor => monitor.peer ? marks.get(monitor.peer) ?? null : ownMark;
        // Keep tiles, and the focused one, while unchanged snapshots arrive. A
        // move keeps its tile too, so arrow keys can move it again.
        const key = JSON.stringify(monitors.map(monitor => [monitor, mark(monitor)]));
        if (key === this._layoutKey) return;
        this._layoutKey = key;
        const ids = new Set(monitors.map(({id}) => id));
        for (const [id, tile] of this._tiles) {
            if (ids.has(id)) continue;
            this._board.remove(tile.button);
            this._tiles.delete(id);
        }
        for (const monitor of monitors) {
            const tile = this._tiles.get(monitor.id) ?? this._newTile(monitor);
            tile.monitor = monitor;
            tile.label.label = tile.button.tooltip_text = monitor.label;
            showMark(tile.mark, mark(monitor));
            tile.button.update_property([Gtk.AccessibleProperty.DESCRIPTION], [`Position ${monitor.x}, ${monitor.y}. Drag it or use the arrow keys to move it.`]);
        }
        this._placeTiles();
    }

    _newTile(monitor) {
        const {id} = monitor;
        const label = new Gtk.Label({ellipsize: Pango.EllipsizeMode.END, max_width_chars: 12});
        const mark = new Gtk.Box({spacing: 2, halign: Gtk.Align.CENTER});
        const content = new Gtk.Box({orientation: Gtk.Orientation.VERTICAL, spacing: 4, valign: Gtk.Align.CENTER});
        content.append(label);
        content.append(mark);
        // This computer's tile is the highlighted one.
        const button = new Gtk.Button({child: content, css_classes: monitor.peer ? [] : ['suggested-action']});
        // Where the tile shows on the board, in pixels.
        const tile = {button, label, mark, monitor, x: 0, y: 0, size: [0, 0], dragging: false};
        const keys = new Gtk.EventControllerKey();
        keys.connect('key-pressed', (_keys, keyval) => {
            const step = ARROWS[keyval];
            if (!step) return false;
            const {x, y} = tile.monitor;
            this._moveTile(id, x + step[0] * KEY_STEP, y + step[1] * KEY_STEP, KEY_TOLERANCE);
            return true;
        });
        button.add_controller(keys);
        this._board.put(button, 0, 0);
        this._tiles.set(id, tile);
        return tile;
    }

    // Scales the layout to fit the box and centers it, as the Mac app does.
    _placeTiles() {
        const tiles = [...this._tiles.values()];
        if (!tiles.length) return;
        const [width, height] = this._boardSize;
        const monitors = tiles.map(tile => tile.monitor);
        const left = Math.min(...monitors.map(m => m.x));
        const top = Math.min(...monitors.map(m => m.y));
        const right = Math.max(...monitors.map(m => m.x + m.width));
        const bottom = Math.max(...monitors.map(m => m.y + m.height));
        const scale = this._scale = Math.min(Math.max(width - 90, 1) / (right - left), Math.max(height - 70, 1) / (bottom - top), 0.12);
        const offsetX = (width - (right - left) * scale) / 2;
        const offsetY = (height - (bottom - top) * scale) / 2;
        // Where layout unit (left, top) is on the board, for computers
        // dropped from the shelf.
        this._origin = {left, top, offsetX, offsetY};
        for (const tile of tiles) {
            const m = tile.monitor;
            const size = tile.size = [Math.max(64, Math.round(m.width * scale)), Math.max(40, Math.round(m.height * scale))];
            tile.x = Math.round(offsetX + (m.x - left + m.width / 2) * scale - size[0] / 2);
            tile.y = Math.round(offsetY + (m.y - top + m.height / 2) * scale - size[1] / 2);
            tile.button.set_size_request(...size);
            if (!tile.dragging) this._board.move(tile.button, tile.x, tile.y);
        }
    }

    // Past a click, the pressed tile follows the pointer.
    _dragTile(drag, dx, dy) {
        const tile = this._pressed;
        if (!tile) return;
        if (!tile.dragging) {
            if (Math.hypot(dx, dy) < 3) return;
            tile.dragging = true;
            // The tile's button lets go of the press.
            drag.set_state(Gtk.EventSequenceState.CLAIMED);
        }
        this._board.move(tile.button, ...this._dragged(tile, dx, dy));
    }

    // Sends where the tile was dropped, in layout units.
    _dropTile(dx, dy) {
        const tile = this._pressed;
        this._pressed = null;
        if (!tile?.dragging) return;
        tile.dragging = false;
        const [x, y] = this._dragged(tile, dx, dy);
        if (tile.found) {
            this._dropFound(tile, x + tile.size[0] / 2, y + tile.size[1] / 2);
            return;
        }
        const {id, x: left, y: top} = tile.monitor;
        const scale = this._scale;
        this._moveTile(id, left + Math.round((x - tile.x) / scale), top + Math.round((y - tile.y) / scale), Math.round(DRAG_TOLERANCE / scale));
    }

    // Where a dragged tile shows: it follows the pointer but stays in the
    // box. A board tile stays on the board; a found one may cross onto it.
    _dragged(tile, dx, dy) {
        const [width, height] = this._boardSize;
        const bottom = tile.found ? height + this._shelfHeight : height;
        const clamp = (value, max) => Math.min(Math.max(value, 0), Math.max(max, 0));
        return [clamp(tile.x + dx, width - tile.size[0]), clamp(tile.y + dy, bottom - tile.size[1])];
    }

    // A found computer let go with its middle at (x, y) on the board joins
    // the layout there, with a new tile's size. Let go on the shelf, it
    // goes back.
    async _dropFound(tile, x, y) {
        if (y >= this._boardSize[1] || !this._origin) {
            this._placeShelf();
            return;
        }
        const {left, top, offsetX, offsetY} = this._origin;
        const scale = this._scale;
        const middle = [left + (x - offsetX) / scale, top + (y - offsetY) / scale];
        await this._place(tile, Math.round(middle[0] - NEW_TILE[0] / 2), Math.round(middle[1] - NEW_TILE[1] / 2), Math.round(DRAG_TOLERANCE / scale));
    }

    // Keyboard people put a found computer beside this one.
    _placeBeside(tile) {
        const local = [...this._tiles.values()].find(({monitor}) => !monitor.peer)?.monitor;
        if (!local || !PLACEABLE.has(tile.found.state)) return;
        this._place(tile, local.x + local.width, local.y, KEY_TOLERANCE);
    }

    async _place(tile, x, y, tolerance) {
        const placed = await this._run({command: 'place', id: tile.found.id, x, y, tolerance});
        // A refused one goes back to the shelf; a placed one leaves it with
        // the next snapshot.
        if (!this._disposed) this._placeShelf();
        return placed;
    }

    _updateFound(found) {
        const key = JSON.stringify(found);
        if (key === this._foundKey) return;
        this._foundKey = key;
        const ids = new Set(found.map(({id}) => id));
        for (const [id, tile] of this._found) {
            if (ids.has(id)) continue;
            if (this._pressed === tile) this._pressed = null;
            this._board.remove(tile.button);
            this._found.delete(id);
        }
        for (const computer of found) {
            const tile = this._found.get(computer.id) ?? this._newFound(computer);
            tile.found = computer;
            const placeable = PLACEABLE.has(computer.state);
            tile.name.label = tile.button.tooltip_text = computer.name;
            tile.detail.label = {
                identifying: 'Looking it up…',
                different_version: 'Different zflow version',
                duplicate_name: 'Same name as another',
            }[computer.state] ?? OS_NAMES[computer.os] ?? '';
            showMark(tile.mark, computer.mark ?? null);
            tile.button.sensitive = placeable;
            tile.button.update_property([Gtk.AccessibleProperty.DESCRIPTION], [placeable
                ? 'Found on your network. Drag it next to a screen, or press Enter to put it beside this computer, to add it.'
                : `Found on your network. ${tile.detail.label}.`]);
        }
        this._placeShelf();
    }

    _newFound(computer) {
        const name = new Gtk.Label({ellipsize: Pango.EllipsizeMode.END, max_width_chars: 14, xalign: 0, css_classes: ['heading']});
        const detail = new Gtk.Label({ellipsize: Pango.EllipsizeMode.END, max_width_chars: 16, xalign: 0, css_classes: ['caption', 'dim-label']});
        const mark = new Gtk.Box({spacing: 2, hexpand: true, halign: Gtk.Align.END});
        const top = new Gtk.Box();
        top.append(new Gtk.Image({icon_name: 'computer-symbolic'}));
        top.append(mark);
        const content = new Gtk.Box({orientation: Gtk.Orientation.VERTICAL, spacing: 2});
        content.append(top);
        content.append(name);
        content.append(detail);
        const button = new Gtk.Button({child: content});
        const tile = {button, name, detail, mark, found: computer, x: 0, y: 0, size: FOUND_SIZE, dragging: false};
        // Only a drag or a key places it, never a stray click.
        const keys = new Gtk.EventControllerKey();
        keys.connect('key-pressed', (_keys, keyval) => {
            if (![Gdk.KEY_Return, Gdk.KEY_KP_Enter, Gdk.KEY_space].includes(keyval)) return false;
            this._placeBeside(tile);
            return true;
        });
        button.add_controller(keys);
        this._board.put(button, 0, 0);
        this._found.set(computer.id, tile);
        return tile;
    }

    // Lays the found computers out in rows under the board.
    _placeShelf() {
        const [width, height] = this._boardSize;
        const tiles = [...this._found.values()];
        const perRow = Math.max(1, Math.floor((width - 2 * SHELF_GAP - 16) / (FOUND_SIZE[0] + SHELF_GAP)));
        const rows = Math.ceil(tiles.length / perRow);
        const shelfHeight = tiles.length ? SHELF_TOP + rows * (FOUND_SIZE[1] + SHELF_GAP) + SHELF_GAP : 0;
        if (shelfHeight !== this._shelfHeight) {
            this._shelfHeight = shelfHeight;
            this._area.content_height = BOARD_HEIGHT + shelfHeight;
        }
        this._shelf.visible = tiles.length > 0;
        this._shelf.set_size_request(Math.max(width - 2 * SHELF_GAP, 1), Math.max(shelfHeight - SHELF_GAP, 1));
        this._board.move(this._shelf, SHELF_GAP, height);
        tiles.forEach((tile, index) => {
            tile.x = 2 * SHELF_GAP + (index % perRow) * (FOUND_SIZE[0] + SHELF_GAP);
            tile.y = height + SHELF_TOP + Math.floor(index / perRow) * (FOUND_SIZE[1] + SHELF_GAP);
            tile.button.set_size_request(...FOUND_SIZE);
            if (!tile.dragging) this._board.move(tile.button, tile.x, tile.y);
        });
    }

    // Tiles stay sensitive during a request, so the focused one keeps focus;
    // a move while another runs is dropped.
    async _moveTile(id, x, y, tolerance) {
        const moved = await this._run({command: 'move_tile', id, x, y, tolerance});
        // A refused move leaves the layout as it was, so put a dragged tile back.
        if (!this._disposed) this._placeTiles();
        return moved;
    }

    _updatePeers(peers) {
        // Keep rows and keyboard focus stable while unchanged snapshots arrive.
        // A keyboard or permission change only moves its control, below.
        const key = JSON.stringify(peers.map(({keyboard: _keyboard, allow_control: _control, reverse_scroll: _scroll, ...peer}) => peer));
        if (key !== this._peerKey) {
            this._peerKey = key;
            for (const row of this._peerRows) this._computers.remove(row);
            this._peerRows = [];
            this._keyboards.clear();
            this._controls.clear();
            this._scrolls.clear();
            for (const {name, detail} of peers) {
                const row = new Adw.ExpanderRow({title: name, subtitle: detail, use_markup: false, expanded: this._expanded.has(name)});
                row.connect('notify::expanded', () => row.expanded ? this._expanded.add(name) : this._expanded.delete(name));
                row.add_prefix(new Gtk.Image({icon_name: 'computer-symbolic'}));
                const control = new Adw.SwitchRow({title: 'Can control this computer'});
                control.connect('notify::active', () => {
                    if (!this._updating) this._run({command: 'set_peer', name, allow_control: control.active});
                });
                row.add_row(control);
                this._controls.set(name, control);
                const keyboard = new Adw.ComboRow({title: 'Keys from this computer', subtitle: 'How its keys act here, from the next time it takes control', model: Gtk.StringList.new(['Standard keys', 'PC key positions', 'Mac shortcuts'])});
                keyboard.connect('notify::selected', () => {
                    if (!this._updating) this._run({command: 'set_peer', name, keyboard: KEYBOARD_MODES[keyboard.selected]});
                });
                row.add_row(keyboard);
                this._keyboards.set(name, keyboard);
                const scroll = new Adw.SwitchRow({title: 'Reverse scrolling', subtitle: 'Turn its scrolling around here'});
                scroll.connect('notify::active', () => {
                    if (!this._updating) this._run({command: 'set_peer', name, reverse_scroll: scroll.active});
                });
                row.add_row(scroll);
                this._scrolls.set(name, scroll);
                // Forgetting puts it back on the shelf, so it takes no asking.
                const forget = new Gtk.Button({icon_name: 'user-trash-symbolic', tooltip_text: `Forget ${name}`, valign: Gtk.Align.CENTER, css_classes: ['flat']});
                forget.connect('clicked', () => this._run({command: 'forget', name}));
                row.add_suffix(forget);
                this._computers.add(row);
                this._peerRows.push(row);
            }
            if (!this._peerRows.length) {
                const row = new Adw.ActionRow({title: 'No computers added yet', subtitle: 'Computers running zflow on this network show up under the arrangement. Drag one next to this computer.', subtitle_lines: 0});
                this._computers.add(row);
                this._peerRows.push(row);
            }
        }
        this._updating = true;
        for (const {name, keyboard: mode, allow_control, reverse_scroll} of peers) {
            const keyboard = this._keyboards.get(name);
            const control = this._controls.get(name);
            const scroll = this._scrolls.get(name);
            keyboard.selected = Math.max(0, KEYBOARD_MODES.indexOf(mode));
            control.active = allow_control;
            scroll.active = !!reverse_scroll;
            keyboard.sensitive = control.sensitive = scroll.sensitive = !this._busy;
        }
        this._updating = false;
    }

    _updateShortcuts(shortcuts) {
        const key = JSON.stringify(shortcuts);
        if (key === this._shortcutKey) return;
        this._shortcutKey = key;
        for (const row of this._shortcutRows ?? []) this._shortcuts.remove(row);
        this._shortcutRows = shortcuts.map(({title, keys}) => {
            const row = new Adw.ActionRow({title, use_markup: false});
            row.add_suffix(new Gtk.Label({label: keys, css_classes: ['dim-label', 'monospace']}));
            this._shortcuts.add(row);
            return row;
        });
        this._shortcuts.visible = shortcuts.length > 0;
    }

    // For a computer mDNS cannot see, such as one on Tailscale. What answers
    // shows up on the shelf.
    _openAddAddress() {
        const dialog = new Adw.AlertDialog({heading: 'Add a Computer by Address', body: 'For a computer zflow can’t find on this network, like one on Tailscale. It shows up in Found on your network; drag it into place to add it.'});
        const entry = new Gtk.Entry({placeholder_text: 'IP address, like 100.64.0.7', activates_default: true, input_purpose: Gtk.InputPurpose.URL});
        dialog.extra_child = entry;
        dialog.add_response('cancel', 'Cancel');
        dialog.add_response('look_up', 'Look Up');
        dialog.set_response_appearance('look_up', Adw.ResponseAppearance.SUGGESTED);
        dialog.default_response = 'look_up';
        dialog.close_response = 'cancel';
        dialog.connect('response', (_dialog, response) => {
            if (response === 'look_up') this._run({command: 'add_address', address: entry.text.trim()});
        });
        this._addDialog = {dialog, entry};
        dialog.present(this.window);
    }

    destroy() {
        this._disposed = true;
        this.client.destroy();
    }
}
