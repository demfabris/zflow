// Run on a private bus: dbus-run-session -- gjs -m tests/gnome_settings_test.js
import Adw from 'gi://Adw?version=1';
import Gdk from 'gi://Gdk?version=4.0';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Gtk from 'gi://Gtk?version=4.0';
import {Settings} from '../packaging/gnome-extension/settings.js';
import {statusText} from '../packaging/gnome-extension/client.js';

function assert(value, message) { if (!value) throw new Error(message); }
function controller(widget, type) {
    const list = widget.observe_controllers();
    for (let i = 0; i < list.get_n_items(); i++) {
        if (list.get_item(i) instanceof type) return list.get_item(i);
    }
    throw new Error(`No ${type.name} on the widget`);
}
function waitFor(predicate) {
    return new Promise((resolve, reject) => {
        let attempts = 0;
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 20, () => {
            if (predicate()) { resolve(); return GLib.SOURCE_REMOVE; }
            if (++attempts > 350) { reject(new Error('Timed out waiting for GTK state')); return GLib.SOURCE_REMOVE; }
            return GLib.SOURCE_CONTINUE;
        });
    });
}

// src/app/api.rs Snapshot, as the agent sends it.
const peer = (name, fields = {}) => ({name, state: 'paired', detail: 'Paired', allow_control: true, keyboard: 'standard', reverse_scroll: false, ...fields});
const ready = {state: 'ready', peer: null, title: 'Ready'};
// src/app/layout_model.rs Layout: this computer's view, where its own tile has no peer.
const layout = {monitors: [
    {id: 'local', label: 'This computer', x: 0, y: 0, width: 2560, height: 1440},
    {id: 'peer:MacBook', label: 'MacBook', peer: 'MacBook', x: 2560, y: 0, width: 1920, height: 1080},
]};
const snapshot = {
    api: 2, status: ready, sharing: true,
    health: [{id: 'service', level: 'ok', title: 'Background service', detail: 'Running', action: null}],
    layout, peers: [peer('MacBook')], pairing: {state: 'idle'}, nearby: [], pause_at_edges: false,
    shortcuts: [{title: 'Return input to this computer', keys: 'Ctrl+Super+Backspace'}],
    autostart: true, config_path: '/etc/zflow/zflow.toml', platform: null,
};
const find = name => snapshot.peers.find(peer => peer.name === name);
let failSharing = false;
let failMove = false;
let callCount = 0;
let keyboardCalls = 0;
let controlCalls = 0;
let scrollCalls = 0;
const moves = [];
const xml = '<node><interface name="io.zflow.Desktop"><method name="Call"><arg type="s" direction="in"/><arg type="s" direction="out"/></method></interface></node>';
const object = Gio.DBusExportedObject.wrapJSObject(xml, {
    Call(json) {
        const request = JSON.parse(json);
        callCount++;
        switch (request.command) {
        case 'snapshot': return JSON.stringify(snapshot);
        case 'set_sharing':
            if (failSharing) throw new Error('Test: service refused the change');
            snapshot.sharing = request.enabled;
            snapshot.status = request.enabled ? ready : {state: 'paused', peer: null, title: 'Paused'};
            break;
        case 'set_autostart': snapshot.autostart = request.enabled; break;
        case 'set_switching': snapshot.pause_at_edges = request.pause_at_edges; break;
        case 'forget': snapshot.peers = snapshot.peers.filter(peer => peer.name !== request.name); break;
        case 'set_peer': {
            const peer = find(request.name);
            const fields = ['keyboard', 'allow_control', 'reverse_scroll'].filter(field => field in request);
            assert(fields.length === 1, 'each control changes one field');
            if ('keyboard' in request) {
                keyboardCalls++;
                peer.keyboard = request.keyboard;
            } else if ('allow_control' in request) {
                controlCalls++;
                peer.allow_control = request.allow_control;
            } else {
                scrollCalls++;
                peer.reverse_scroll = request.reverse_scroll;
            }
            break;
        }
        case 'pair':
            if (request.address === null) {
                snapshot.pairing = {state: 'listening', code: '482 913'};
            } else {
                assert(request.address === '192.0.2.7' && request.code === '123456', 'connecting forwards the address and the typed code');
                snapshot.pairing = {state: 'paired', name: 'Mac mini'};
                snapshot.peers.push(peer('Mac mini'));
            }
            break;
        case 'pair_respond':
            assert(snapshot.pairing.state === 'confirm', 'answers only a waiting question');
            if (request.allow) {
                snapshot.pairing = {state: 'paired', name: snapshot.pairing.name};
                snapshot.peers.push(peer(snapshot.pairing.name));
            } else {
                snapshot.pairing = {state: 'failed', error: 'Pairing declined'};
            }
            break;
        case 'pair_cancel': snapshot.pairing = {state: 'idle'}; break;
        case 'move_tile': {
            moves.push(request);
            if (failMove) throw new Error('Computers cannot overlap');
            Object.assign(layout.monitors.find(monitor => monitor.id === request.id), {x: request.x, y: request.y});
            break;
        }
        default: throw new Error(`Unexpected request ${request.command}`);
        }
        return JSON.stringify({ok: true});
    },
});
object.export(Gio.DBus.session, '/io/zflow/Desktop');
let owned = false;
const owner = Gio.bus_own_name_on_connection(Gio.DBus.session, 'io.zflow.Desktop', Gio.BusNameOwnerFlags.NONE, () => { owned = true; }, null);
let failure = null;
const app = new Adw.Application({application_id: 'io.zflow.SettingsTest'});
app.connect('activate', () => {
    const window = new Adw.ApplicationWindow({application: app, default_width: 520, default_height: 640});
    const settings = new Settings(window);
    window.content = settings.page;
    window.present();
    (async () => {
        await waitFor(() => owned && settings._sharing.sensitive);
        assert(settings._status.title === 'Ready', 'ready status');
        assert(settings._healthRows[0].title === 'Background service' && settings._health.visible, 'checks that pass stay listed');
        assert(settings._shortcutRows[0].title === 'Return input to this computer', 'shortcuts are listed');
        assert(settings._help.description.includes('/etc/zflow/zflow.toml'), 'the advanced settings file is named');
        const firstRow = settings._peerRows[0];
        await settings.client.refresh();
        assert(settings._peerRows[0] === firstRow, 'refresh must preserve keyboard focus');
        // The layout: tiles for both computers, with this one marked.
        assert(settings._layout.visible && settings._computers.description.startsWith('Drag'), 'a layout replaces the hint about the other computer');
        const mac = settings._tiles.get('peer:MacBook').button;
        const local = settings._tiles.get('local').button;
        assert(settings._tiles.get('peer:MacBook').label.label === 'MacBook' && mac.tooltip_text === 'MacBook', 'tiles are labelled');
        assert(local.has_css_class('suggested-action') && !mac.has_css_class('suggested-action'), 'this computer is marked');
        await settings.client.refresh();
        assert(settings._tiles.get('peer:MacBook').button === mac, 'an unchanged snapshot keeps the tiles');
        // An arrow key moves the focused tile 100 units and snaps within 150, as on the Mac.
        mac.grab_focus();
        assert(controller(mac, Gtk.EventControllerKey).emit('key-pressed', Gdk.KEY_Right, 0, 0), 'the tile takes arrow keys');
        await waitFor(() => moves.length === 1 && !settings._busy);
        assert(JSON.stringify(moves[0]) === JSON.stringify({command: 'move_tile', id: 'peer:MacBook', x: 2660, y: 0, tolerance: 150}), `the key sends one move: ${JSON.stringify(moves)}`);
        await waitFor(() => settings._tiles.get('peer:MacBook').monitor.x === 2660);
        assert(settings._tiles.get('peer:MacBook').button === mac && window.get_focus() === mac, 'a moved tile keeps focus');
        // A drag sends where the tile was dropped, in layout units.
        const drag = controller(settings._board, Gtk.GestureDrag);
        const press = () => {
            const {x, y} = settings._tiles.get('peer:MacBook');
            drag.emit('drag-begin', x + 5, y + 5);
        };
        press();
        drag.emit('drag-update', 1, 1);
        drag.emit('drag-end', 1, 1);
        // A drag that starts beside the tiles moves nothing either.
        drag.emit('drag-begin', 1, 1);
        drag.emit('drag-update', 20, 20);
        drag.emit('drag-end', 20, 20);
        assert(moves.length === 1 && !settings._busy, 'a click or an empty drag sends nothing');
        const scale = settings._scale;
        press();
        drag.emit('drag-update', -30, 12);
        drag.emit('drag-end', -30, 12);
        await waitFor(() => moves.length >= 2 && !settings._busy);
        const dropped = {command: 'move_tile', id: 'peer:MacBook', x: 2660 + Math.round(-30 / scale), y: Math.round(12 / scale), tolerance: Math.round(14 / scale)};
        assert(moves.length === 2 && JSON.stringify(moves[1]) === JSON.stringify(dropped), `a drag sends one move: ${JSON.stringify(moves)}`);
        // A refused move puts the tile back. Times out if it stays where it was dropped.
        await waitFor(() => settings._tiles.get('peer:MacBook').monitor.x === dropped.x);
        failMove = true;
        const tile = settings._tiles.get('peer:MacBook');
        const home = [tile.x, tile.y].join();
        // Where the tile shows now; get_child_position gives the last allocation instead.
        const shown = () => settings._board.get_child_transform(mac).to_translate().join();
        press();
        drag.emit('drag-update', -40, 0);
        assert(shown() !== home, 'the tile follows the drag');
        drag.emit('drag-end', -40, 0);
        await waitFor(() => moves.length === 3 && !settings._busy && shown() === home);
        assert(settings._error.subtitle === 'Computers cannot overlap' && tile.monitor.x === dropped.x, 'the refusal is shown and the layout stays');
        failMove = false;
        const keyboard = settings._keyboards.get('MacBook');
        assert(keyboard.selected === 0, 'a peer without a keyboard mode shows standard keys');
        keyboard.selected = 2;
        assert(!keyboard.sensitive, 'a request in flight locks the dropdown instead of dropping a choice');
        // Times out if the dropdown stays locked after the request.
        await waitFor(() => find('MacBook').keyboard === 'mac' && !settings._busy && keyboard.sensitive);
        keyboard.selected = 0;
        await waitFor(() => find('MacBook').keyboard === 'standard' && !settings._busy);
        assert(keyboardCalls === 2, 'each choice sends set_peer once');
        find('MacBook').keyboard = 'pc_positions';
        await waitFor(() => keyboard.selected === 1);
        assert(keyboardCalls === 2, 'a snapshot moves the dropdown without sending a request');
        assert(settings._peerRows[0] === firstRow && settings._keyboards.get('MacBook') === keyboard, 'a keyboard change keeps the row');
        const control = settings._controls.get('MacBook');
        assert(control.active, 'a paired computer can control this one');
        control.active = false;
        assert(!control.sensitive, 'a request in flight locks the switch');
        await waitFor(() => !find('MacBook').allow_control && !settings._busy && control.sensitive);
        find('MacBook').allow_control = true;
        await waitFor(() => control.active);
        assert(controlCalls === 1, 'a snapshot moves the switch without sending a request');
        assert(settings._peerRows[0] === firstRow, 'a permission change keeps the row');
        assert(!settings._pause.active && settings._pause.sensitive, 'crossings do not pause by default');
        settings._pause.active = true;
        await waitFor(() => snapshot.pause_at_edges && !settings._busy && settings._pause.sensitive);
        const scroll = settings._scrolls.get('MacBook');
        assert(!scroll.active, 'scrolling starts the right way round');
        scroll.active = true;
        await waitFor(() => find('MacBook').reverse_scroll && !settings._busy && scroll.sensitive);
        assert(scrollCalls === 1 && settings._peerRows[0] === firstRow, 'reversing scroll sends set_peer once and keeps the row');
        firstRow.expanded = true;
        Object.assign(find('MacBook'), {state: 'controlled_from_here', detail: 'Controlled from here'});
        await waitFor(() => settings._peerRows[0] !== firstRow);
        assert(settings._peerRows[0].expanded, 'a rebuilt row stays open');
        assert(settings._peerRows[0].subtitle === 'Controlled from here', 'the row shows the state the agent sends');
        Object.assign(find('MacBook'), {state: 'connected', detail: 'Connected'});
        settings._sharing.active = false;
        await waitFor(() => !snapshot.sharing && !settings._busy);
        assert(settings._status.title === 'Paused', 'pause reaches daemon and refreshes status');
        failSharing = true;
        settings._sharing.active = true;
        await waitFor(() => !settings._busy && settings._errorGroup.visible);
        assert(!settings._sharing.active, 'rejected toggle rolls back');
        assert(settings._error.subtitle === 'Test: service refused the change', 'service errors hide the D-Bus error name');
        failSharing = false;
        settings._login.active = false;
        await waitFor(() => !snapshot.autostart && !settings._busy);
        assert(!settings._pairing, 'a computer with peers does not open pairing by itself');
        settings._openPairing();
        await waitFor(() => settings._pairing.code.label === '482 913' && !settings._busy);
        settings._pairing.remote.text = '192.0.2.7';
        settings._pairing.entered.text = '123 456';
        settings._pairing.connect.emit('clicked');
        await waitFor(() => snapshot.pairing.state === 'paired' && !settings._busy);
        assert(find('Mac mini'), 'pairing saves the other computer');
        await waitFor(() => settings._pairing.stage.title === 'Paired with Mac mini');
        settings._pairing.dialog.close();
        await waitFor(() => settings._pairing === null && snapshot.pairing.state === 'idle');
        await settings._run({command: 'forget', name: 'MacBook'});
        await settings._run({command: 'forget', name: 'Mac mini'});
        assert(!find('MacBook'), 'forget uses daemon API');
        const freshWindow = new Adw.ApplicationWindow({application: app, default_width: 520, default_height: 640});
        const fresh = new Settings(freshWindow);
        freshWindow.content = fresh.page;
        freshWindow.present();
        await waitFor(() => fresh._pairing !== null && snapshot.pairing.state === 'listening' && fresh._pairing.code.label === '482 913');
        // A computer proves the code; nothing is saved until the person here allows it.
        snapshot.pairing = {state: 'confirm', name: 'Stranger', address: '192.0.2.66'};
        await fresh.client.refresh();
        await waitFor(() => fresh._pairing.ask.visible && !fresh._pairing.shown.visible);
        assert(fresh._pairing.question.title === 'Allow Stranger to pair with this computer?', 'the question names the computer');
        assert(fresh._pairing.question.subtitle.includes('192.0.2.66'), 'the question shows its address');
        await fresh._respond(false);
        await waitFor(() => snapshot.pairing.state === 'failed' && fresh._pairing.renew.visible && !fresh._busy);
        assert(!find('Stranger'), 'declining saves nothing');
        await fresh._listen();
        await waitFor(() => snapshot.pairing.state === 'listening' && !fresh._busy);
        snapshot.pairing = {state: 'confirm', name: 'MacBook', address: '192.0.2.9'};
        await fresh.client.refresh();
        await waitFor(() => fresh._pairing.ask.visible);
        fresh._pairing.allow.emit('clicked');
        await waitFor(() => snapshot.pairing.state === 'paired' && !fresh._busy);
        assert(find('MacBook'), 'allowing saves the computer');
        await waitFor(() => fresh._pairing.stage.title === 'Paired with MacBook');
        snapshot.peers = [];
        fresh._pairing.dialog.close();
        await waitFor(() => snapshot.pairing.state === 'idle');
        fresh.destroy();
        freshWindow.close();
        Object.assign(snapshot, {
            sharing: null, status: {state: 'attention', peer: null, title: 'Needs attention'}, shortcuts: [], layout: null,
            health: [{id: 'service', level: 'error', title: 'Background service', detail: 'Start the zflow system service', action: null}],
        });
        await settings.client.refresh();
        assert(!settings._sharing.sensitive && !settings._pairButton.sensitive, 'offline controls disabled');
        assert(!settings._layout.visible && settings._computers.description === 'Arrange computers in zflow on the other computer.', 'without a layout, the hint comes back');
        assert(statusText(snapshot) === 'Needs attention', 'offline status');
        assert(settings._healthRows[0].subtitle === 'Start the zflow system service', 'the check says what failed');
        assert(!settings._shortcuts.visible, 'no shortcuts without the service');
        snapshot.api = 1;
        await settings.client.refresh();
        assert(settings._status.title === 'Update zflow' && settings._errorGroup.visible, 'an agent from another API level asks for an update');
        settings.destroy();
        await waitFor(() => !settings.client._polling);
        const before = callCount;
        await settings.client.refresh();
        assert(callCount === before, 'closed window stops polling');
        print('GTK settings: status, checks, shortcuts, focus, layout moves, keyboard mode, control permission, reverse scrolling, pause at edges, open rows, pause, rollback, login, pairing, first-run pairing, allow and decline, forget, offline and cleanup passed');
    })().catch(error => { failure = error; printerr(error.stack); }).finally(() => {
        settings.destroy();
        object.unexport();
        Gio.bus_unown_name(owner);
        app.quit();
    });
});
app.run([]);
if (failure) throw failure;
