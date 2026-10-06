import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const timers = new Map();
const calls = [];
// While true, the desktop agent is not running yet, as right after login.
let agentDown = false;
let snapshot = {api: 5, status: {state: 'controlled', peer: 'Mac', title: 'Controlled by Mac'}, sharing: true, health: []};
const context = {
    GLib: {
        Variant: class { constructor(_type, value) { this.value = value; } },
        VariantType: class {}, Error: class extends Error {}, PRIORITY_DEFAULT: 0, SOURCE_CONTINUE: true,
        timeout_add(_priority, _interval, callback) { const id = timers.size + 1; timers.set(id, callback); return id; },
        Source: {remove(id) { timers.delete(id); }},
    },
    Gio: {
        DBusCallFlags: {NONE: 0, NO_AUTO_START: 1},
        Cancellable: class { cancel() { this.cancelled = true; } is_cancelled() { return !!this.cancelled; } },
        DBus: {session: {
            call(_bus, _path, _interface, _method, parameters, _type, flags, _timeout, cancel, callback) {
                const request = JSON.parse(parameters.value[0]);
                calls.push({request, flags});
                if (request.command === 'set_sharing') {
                    snapshot.sharing = request.enabled;
                    snapshot.status = {state: 'paused', peer: null, title: 'Paused'};
                }
                queueMicrotask(() => callback({call_finish() {
                    if (cancel.is_cancelled()) throw new Error('cancelled');
                    if (agentDown) throw new Error('The name io.zflow.Desktop was not provided');
                    return {deep_unpack: () => [JSON.stringify(request.command === 'snapshot' ? snapshot : {ok: true})]};
                }}, {}));
            },
        }},
    },
    St: {Icon: class { constructor(props) {Object.assign(this, props);} }},
    PanelMenu: {Button: class {
        menu = {addMenuItem() {}, addAction() {}};
        handlers = {};
        add_child() {}
        connect(signal, callback) {this.handlers[signal] = callback;}
        destroy() {this.destroyed = (this.destroyed ?? 0) + 1; this.handlers.destroy?.();}
    }},
    PopupMenu: {
        PopupMenuItem: class {label = {};}, PopupSeparatorMenuItem: class {},
        PopupSwitchMenuItem: class {
            constructor(_text, active) {this.state = active; this.handlers = [];}
            setSensitive(value) {this.sensitive = value;}
            // GNOME 50 emits toggled when code sets the state, as for a click.
            setToggleState(value) {if (value !== this.state) this.click();}
            click() {this.state = !this.state; for (const handler of this.handlers) handler(this, this.state);}
            connect(_signal, callback) {this.handlers.push(callback);}
        },
    },
    Main: {panel: {addToStatusArea() {}}, notifyError() {throw new Error('Unexpected panel error');}},
};
for (const file of ['client', 'indicator']) {
    const source = fs.readFileSync(new URL(`../packaging/gnome-extension/${file}.js`, import.meta.url), 'utf8')
        .replace(/^import .*;\r?\n/gm, '').replace(/^export /gm, '');
    vm.runInNewContext(source + (file === 'client' ? '\nglobalThis.TestClient = Client;' : '\nglobalThis.TestIndicator = Indicator;'), context);
}
const panel = new context.TestIndicator();
await new Promise(setImmediate);
assert.equal(calls[0].flags, 1, 'panel reads must not override disabled login startup');
assert.equal(panel._status.label.text, 'Controlled by Mac');
assert.equal(panel._sharing.state, true);
assert.equal(panel._icon.icon_name, 'io.zflow.zflow-symbolic');
// After a reboot the panel can start before the agent, then meet one from
// another API level, and then a working one. Showing any of that is not a
// request, so sharing stays as it was.
const sharingRequests = () => calls.filter(call => call.request.command === 'set_sharing')
    .map(call => call.request.enabled);
agentDown = true;
await panel._client.refresh();
assert.equal(panel._status.label.text, 'zflow is not running');
assert.equal(panel._sharing.state, false);
agentDown = false;
snapshot.sharing = null;
await panel._client.refresh();
snapshot.api = 6;
await panel._client.refresh();
assert.equal(panel._status.label.text, 'Update zflow');
snapshot.api = 5;
snapshot.sharing = true;
await panel._client.refresh();
assert.equal(panel._sharing.state, true);
assert.deepEqual(sharingRequests(), [], 'showing status never changes sharing');
// A person's click does.
panel._sharing.click();
await new Promise(setImmediate);
assert.deepEqual(sharingRequests(), [false]);
assert.equal(panel._status.label.text, 'Paused');
assert.equal(panel._sharing.state, false);
assert.equal(panel._icon.icon_name, 'media-playback-pause-symbolic');
// An agent from another API level gets "Update zflow" instead of a broken menu.
snapshot.api = 6;
await panel._client.refresh();
assert.equal(panel._status.label.text, 'Update zflow');
assert.equal(panel._icon.icon_name, 'dialog-warning-symbolic');
snapshot.api = 5;
await panel._client.refresh();
assert.equal(panel._status.label.text, 'Paused');
// A failing check warns even while the menu has nothing else to say.
snapshot.health = [{id: 'service', level: 'error', title: 'Background service', detail: 'down', action: null}];
snapshot.sharing = null;
await panel._client.refresh();
assert.equal(panel._icon.icon_name, 'dialog-warning-symbolic');
assert.equal(panel._sharing.sensitive, false);
panel.destroy();
assert.equal(timers.size, 0, 'disable removes panel polling');
assert.ok(panel._button.destroyed);
// Shell destroys the panel at session end without calling disable().
const ended = new context.TestIndicator();
Object.freeze(ended._status.label);
ended._button.destroy();
await new Promise(setImmediate);
assert.equal(timers.size, 0, 'session teardown stops panel polling before it reaches disposed widgets');
ended.destroy();
assert.equal(ended._button.destroyed, 1, 'a later disable leaves the disposed button alone');
const settings = new context.TestClient(() => {}, true);
await settings.refresh();
assert.equal(calls.at(-1).flags, 0, 'opening preferences explicitly starts the session agent');
await settings.refresh();
assert.equal(calls.at(-1).flags, 1, 'subsequent reads do not keep restarting a stopped agent');
settings.destroy();
console.log('GNOME panel status, pause, API check, activation policy and cleanup checks passed');
