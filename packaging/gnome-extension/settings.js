import Adw from 'gi://Adw?version=1';
import Gdk from 'gi://Gdk?version=4.0';
import Gtk from 'gi://Gtk?version=4.0';
import Pango from 'gi://Pango';
import {Client, compatible, statusText} from './client.js';

// src/core/keymap.rs KeyboardMode, in dropdown order.
const KEYBOARD_MODES = ['standard', 'pc_positions', 'mac'];
const HEALTH_ICONS = {ok: 'object-select-symbolic', warning: 'dialog-warning-symbolic', error: 'dialog-error-symbolic'};
const NO_LAYOUT = 'Arrange computers in zflow on the other computer.';
const LAYOUT_HINT = 'Drag computers to match your desk. The pointer crosses where two touch.';
// As in the Mac app (ComputerLayout.swift): an arrow key moves a tile 100
// layout units and snaps within 150; a drag snaps within 14 pixels.
const KEY_STEP = 100;
const KEY_TOLERANCE = 150;
const DRAG_TOLERANCE = 14;
const ARROWS = {[Gdk.KEY_Left]: [-1, 0], [Gdk.KEY_Right]: [1, 0], [Gdk.KEY_Up]: [0, -1], [Gdk.KEY_Down]: [0, 1]};

function button(label, action, css = []) {
    const widget = new Gtk.Button({label, valign: Gtk.Align.CENTER, css_classes: css});
    widget.connect('clicked', action);
    return widget;
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
        this.page = new Adw.PreferencesPage({title: 'zflow', icon_name: 'input-mouse-symbolic'});
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

        this._computers = new Adw.PreferencesGroup({title: 'Computers', description: NO_LAYOUT});
        this._pairButton = button('Pair Computer…', () => this._openPairing(), ['suggested-action']);
        this._computers.header_suffix = this._pairButton;
        // The layout comes first, above the computers' rows. Each tile is a
        // button, so it takes keyboard focus and has a name.
        this._tiles = new Map();
        this._layoutKey = '';
        this._pressed = null;
        this._boardSize = [440, 200];
        this._board = new Gtk.Fixed();
        // One drag for the whole box. On a tile, it would measure from the
        // tile that moves under it. It sees a press before the tile's button
        // does, as a scrolled window's drag does, and takes it only once the
        // pointer moves.
        const drag = new Gtk.GestureDrag({propagation_phase: Gtk.PropagationPhase.CAPTURE});
        drag.connect('drag-begin', (_drag, x, y) => {
            // Later tiles draw on top.
            this._pressed = [...this._tiles.values()].findLast(tile => x >= tile.x && y >= tile.y && x < tile.x + tile.size[0] && y < tile.y + tile.size[1]);
        });
        drag.connect('drag-update', (_drag, dx, dy) => this._dragTile(drag, dx, dy));
        drag.connect('drag-end', (_drag, dx, dy) => this._dropTile(dx, dy));
        this._board.add_controller(drag);
        // A Gtk.Fixed does not report its width, so the empty area under it does.
        const area = new Gtk.DrawingArea({content_height: this._boardSize[1], hexpand: true});
        area.connect('resize', (_area, width, height) => {
            this._boardSize = [width, height];
            this._placeTiles();
        });
        const canvas = new Gtk.Overlay({child: area, overflow: Gtk.Overflow.HIDDEN});
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
        this._sharing.sensitive = this._login.sensitive = this._pairButton.sensitive = this._pause.sensitive = false;
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
        this._login.active = (known && snapshot.autostart) ?? false;
        this._login.sensitive = known && snapshot.autostart !== null && !this._busy;
        this._pairButton.sensitive = online && !this._busy && !this._pairing;
        this._updating = false;
        this._error.subtitle = this._actionError || (known ? '' : snapshot?.error ?? 'Update zflow so this window and the service match.');
        this._errorGroup.visible = !!this._error.subtitle;
        this._help.description = `Changes save automatically. Advanced settings are in ${known ? snapshot.config_path : '/etc/zflow/zflow.toml'}.`;
        this._updateHealth(known ? snapshot.health : []);
        this._updateLayout(known ? snapshot.layout : null);
        this._updatePeers(known ? snapshot.peers : []);
        this._updateShortcuts(known ? snapshot.shortcuts : []);
        // A fresh install opens pairing with its code on screen, so the other
        // computer can pair without anyone clicking through settings here.
        if (online && !this._checkedFirstRun) {
            this._checkedFirstRun = true;
            if (!snapshot.peers.length) this._openPairing();
        }
        this._updatePairing(known ? snapshot : {});
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

    _updateLayout(layout) {
        const monitors = layout?.monitors ?? [];
        this._layout.visible = monitors.length > 0;
        this._computers.description = monitors.length ? LAYOUT_HINT : NO_LAYOUT;
        // Keep tiles, and the focused one, while unchanged snapshots arrive. A
        // move keeps its tile too, so arrow keys can move it again.
        const key = JSON.stringify(monitors);
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
            tile.button.update_property([Gtk.AccessibleProperty.DESCRIPTION], [`Position ${monitor.x}, ${monitor.y}. Drag it or use the arrow keys to move it.`]);
        }
        this._placeTiles();
    }

    _newTile(monitor) {
        const {id} = monitor;
        const label = new Gtk.Label({ellipsize: Pango.EllipsizeMode.END, max_width_chars: 12});
        // This computer's tile is the highlighted one.
        const button = new Gtk.Button({child: label, css_classes: monitor.peer ? [] : ['suggested-action']});
        // Where the tile shows on the board, in pixels.
        const tile = {button, label, monitor, x: 0, y: 0, size: [0, 0], dragging: false};
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
        const {id, x: left, y: top} = tile.monitor;
        const scale = this._scale;
        this._moveTile(id, left + Math.round((x - tile.x) / scale), top + Math.round((y - tile.y) / scale), Math.round(DRAG_TOLERANCE / scale));
    }

    // Where a dragged tile shows: it follows the pointer but stays in the box.
    _dragged(tile, dx, dy) {
        const [width, height] = this._boardSize;
        const clamp = (value, max) => Math.min(Math.max(value, 0), Math.max(max, 0));
        return [clamp(tile.x + dx, width - tile.size[0]), clamp(tile.y + dy, height - tile.size[1])];
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
                const keyboard = new Adw.ComboRow({title: 'Keys from this computer', subtitle: 'How its keys act here', model: Gtk.StringList.new(['Standard keys', 'PC key positions', 'Mac shortcuts'])});
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
                const forget = new Gtk.Button({icon_name: 'user-trash-symbolic', tooltip_text: `Forget ${name}`, valign: Gtk.Align.CENTER, css_classes: ['flat']});
                forget.connect('clicked', () => this._forget(name));
                row.add_suffix(forget);
                this._computers.add(row);
                this._peerRows.push(row);
            }
            if (!this._peerRows.length) {
                const row = new Adw.ActionRow({title: 'No paired computers', subtitle: 'Pair another computer to get started.'});
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

    _forget(name) {
        const dialog = new Adw.AlertDialog({heading: `Forget ${name}?`, body: 'This stops input between the two computers and removes its trusted identity. Pair it again to reconnect.'});
        dialog.add_response('cancel', 'Cancel');
        dialog.add_response('forget', 'Forget Computer');
        dialog.set_response_appearance('forget', Adw.ResponseAppearance.DESTRUCTIVE);
        dialog.default_response = dialog.close_response = 'cancel';
        dialog.connect('response', (_dialog, response) => {
            if (response === 'forget') this._run({command: 'forget', name});
        });
        dialog.present(this.window);
    }

    _openPairing() {
        if (this._pairing) return;
        const dialog = new Adw.Dialog({title: 'Pair Computer', content_width: 440, content_height: 560});
        const toolbar = new Adw.ToolbarView();
        toolbar.add_top_bar(new Adw.HeaderBar());
        const page = new Adw.PreferencesPage();
        toolbar.content = page;
        dialog.child = toolbar;
        // Knowing the code is not enough: the person here allows the computer.
        const ask = new Adw.PreferencesGroup({visible: false});
        const question = new Adw.ActionRow({title: '', subtitle_lines: 0, use_markup: false});
        question.add_prefix(new Gtk.Image({icon_name: 'computer-symbolic'}));
        ask.add(question);
        const allow = button('Allow', () => this._respond(true), ['suggested-action']);
        const answers = new Gtk.Box({spacing: 12, halign: Gtk.Align.END, margin_top: 12});
        answers.append(button('Decline', () => this._respond(false)));
        answers.append(allow);
        ask.add(answers);
        page.add(ask);
        const shown = new Adw.PreferencesGroup({title: 'Setup code', description: 'On the other computer, start pairing in zflow and type this code.'});
        const code = new Gtk.Label({label: '', selectable: true, css_classes: ['title-1', 'numeric'], margin_top: 18, margin_bottom: 18});
        shown.add(code);
        const renew = button('Show a New Code', () => this._listen());
        shown.add(renew);
        page.add(shown);
        const other = new Adw.PreferencesGroup({title: 'Or type another computer’s code', description: 'Open Pair Computer on the other computer, then enter its address and the code it shows.'});
        const remote = new Adw.EntryRow({title: 'IP address'});
        const entered = new Adw.EntryRow({title: 'Its setup code', input_purpose: Gtk.InputPurpose.DIGITS});
        const connect = button('Pair', () => this._connect(remote.text, entered.text), ['suggested-action']);
        other.add(remote);
        other.add(entered);
        other.add(connect);
        page.add(other);
        const nearby = new Adw.PreferencesGroup({title: 'Nearby computers'});
        page.add(nearby);
        const result = new Adw.PreferencesGroup();
        // Peer names and remote error text end up here, so never parse them as markup.
        const stage = new Adw.ActionRow({title: '', visible: false, subtitle_lines: 0, use_markup: false});
        result.add(stage);
        page.add(result);
        this._pairing = {dialog, ask, question, allow, shown, other, code, renew, remote, entered, connect, stage, nearby, rows: [], nearbyKey: '', asked: false};
        dialog.connect('closed', () => {
            this._pairing = null;
            if (this._pairOwned) {
                this._pairOwned = false;
                this.client.call({command: 'pair_cancel'}).catch(error => this._showError(error.message));
            }
            this.client.refresh();
        });
        dialog.present(this.window);
        this._updatePairing(this.client.snapshot ?? {});
        // Showing a code is the usual case: the Mac types it.
        this._listen();
    }

    _listen() {
        return this._startPair({command: 'pair', address: null});
    }

    _respond(allow) {
        return this._run({command: 'pair_respond', allow});
    }

    _connect(remote, code) {
        remote = remote.trim();
        if (!remote) { this._showError('Enter the other computer’s IP address.'); return Promise.resolve(false); }
        return this._startPair({command: 'pair', address: remote, code: code.replace(/[\s-]/g, '')});
    }

    async _startPair(request) {
        // One pairing runs at a time, so a new request replaces the code on screen.
        if (this._pairOwned) {
            this._pairOwned = false;
            await this.client.call({command: 'pair_cancel'}).catch(() => {});
        }
        this._pendingPair = this._run(request);
        const started = await this._pendingPair;
        this._pendingPair = null;
        if (started && (!this._pairing || this._disposed)) {
            await this.client.call({command: 'pair_cancel'}).catch(() => {});
            return false;
        }
        this._pairOwned = started;
        return started;
    }

    _updatePairing(snapshot) {
        const ui = this._pairing;
        if (!ui) return;
        const pairing = snapshot.pairing ?? {state: 'idle'};
        const listening = pairing.state === 'listening';
        const confirming = pairing.state === 'confirm';
        const connecting = pairing.state === 'connecting' || pairing.state === 'approving';
        ui.ask.visible = confirming;
        ui.shown.visible = ui.other.visible = !confirming;
        if (confirming) {
            ui.question.title = `Allow ${pairing.name ?? 'this computer'} to pair with this computer?`;
            ui.question.subtitle = `It entered this computer’s code from ${pairing.address ?? 'your network'}. Once paired, each can control the other. Allow it only if it is the computer you are setting up.`;
            if (!ui.asked) ui.allow.grab_focus();
        }
        ui.asked = confirming;
        ui.code.label = listening ? (pairing.code ?? '…') : 'No code shown';
        ui.code.sensitive = listening;
        ui.renew.visible = !listening && !connecting && !confirming;
        ui.connect.sensitive = !connecting && !this._busy;
        const issue = pairing.error || this._actionError;
        ui.stage.visible = connecting || pairing.state === 'paired' || !!issue;
        ui.stage.title = pairing.state === 'approving' ? 'Waiting for the other computer…' : connecting ? 'Pairing…' : pairing.state === 'paired' ? `Paired with ${pairing.name ?? 'the other computer'}` : 'Pairing needs attention';
        ui.stage.subtitle = pairing.state === 'paired' ? 'You can close this window. Arrange the computers in zflow on the other computer.'
            : pairing.state === 'approving' ? 'Choose Allow on the other computer.' : connecting ? '' : issue || '';
        const discovery = snapshot.health?.find(row => row.id === 'discovery')?.detail;
        const key = JSON.stringify([snapshot.nearby, discovery]);
        if (key !== ui.nearbyKey) {
            ui.nearbyKey = key;
            for (const row of ui.rows) ui.nearby.remove(row);
            ui.rows = [];
            for (const record of snapshot.nearby ?? []) {
                const address = record.addresses[0];
                const separator = address.lastIndexOf(':');
                const row = new Adw.ActionRow({title: address.slice(0, separator), subtitle: record.compatible ? 'Available on your network' : 'Update zflow on this computer', use_markup: false});
                const use = button('Use', () => {
                    ui.remote.text = record.pair_address;
                    ui.entered.grab_focus();
                });
                use.sensitive = record.compatible && !!record.pair_address;
                row.add_suffix(use);
                ui.nearby.add(row);
                ui.rows.push(row);
            }
            if (!ui.rows.length) {
                const row = new Adw.ActionRow({title: 'No other computers found', subtitle: discovery || 'You can enter an address instead.', subtitle_lines: 0});
                ui.nearby.add(row);
                ui.rows.push(row);
            }
        }
    }

    destroy() {
        this._disposed = true;
        // Let the cancellation reach the service before cancelling pending UI reads.
        if (this._pendingPair) this._pendingPair.then(started => started ? this.client.call({command: 'pair_cancel'}) : null).finally(() => this.client.destroy()).catch(() => {});
        else if (this._pairOwned) this.client.call({command: 'pair_cancel'}).finally(() => this.client.destroy()).catch(() => {});
        else this.client.destroy();
    }
}
