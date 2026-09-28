import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// Execute the shipped extension with deterministic compositor operations.
const source = fs.readFileSync(new URL('../packaging/gnome-extension/extension.js', import.meta.url), 'utf8')
    .replace(/^import .*;\n/gm, '').replace('export default class ', 'class ') + '\nglobalThis.TestExtension = ZflowExtension;';

function desktop(monitors = [{x: 0, y: 0, width: 1920, height: 1080}]) {
    let pointer = {x: monitors[0].x + 100, y: monitors[0].y + 100};
    let now = 1;
    const barriers = [];
    const timers = new Map();
    const handlers = new Map();
    let next = 1;
    const seat = {warp_pointer(x, y) {pointer = {x, y};}};
    const backend = {get_default_seat: () => seat};
    const watch = {};
    const emitted = [];
    const display = {focus_window: null, handlers: new Map(),
        connect(name, fn) {this.handlers.set(name, fn); return 3;}, disconnect(id) {this.disconnected = id;}};
    // GNOME 51 reaches the backend only through the stage context.
    const context = {
        API: 2,
        Extension: class {},
        Indicator: class { destroy() {} },
        global: {backend: {capabilities: 1}, stage: {get_context: () => ({get_backend: () => backend})},
            display, get_pointer: () => [pointer.x, pointer.y]},
        Main: {layoutManager: {monitors, connect(name, fn) {handlers.set(name, fn); return 1;}, disconnect() {}}},
        GLib: {
            PRIORITY_DEFAULT: 0, PRIORITY_DEFAULT_IDLE: 0, SOURCE_CONTINUE: true, SOURCE_REMOVE: false,
            get_monotonic_time: () => now,
            timeout_add(_priority, interval, fn) {const id = next++; timers.set(id, {fn, interval, due: now + interval * 1000}); return id;},
            idle_add(_priority, fn) {queueMicrotask(fn); return next++;},
            Source: {remove(id) {timers.delete(id);}},
            Variant: class { constructor(_type, value) {this.value = value;} },
        },
        Gio: {DBus: {session: {emit_signal(...args) {emitted.push(args);}}}, DBusExportedObject: {wrapJSObject() {return {export() {}, unexport() {}};}},
            BusNameOwnerFlags: {NONE: 0}, bus_own_name_on_connection: () => 1, bus_unown_name() {},
            // The desktop agent is already running as :1.7.
            BusNameWatcherFlags: {NONE: 0}, bus_unwatch_name(id) {watch.removed = id;},
            bus_watch_name_on_connection(_connection, name, _flags, appeared, vanished) {
                Object.assign(watch, {name, appeared, vanished});
                appeared(null, name, ':1.7');
                return 2;
            }},
        Clutter: {},
        Shell: {WindowTracker: {get_default: () => ({get_window_app: win => win.app})}},
        Meta: {
            BackendCapabilities: {BARRIERS: 1},
            BarrierDirection: {POSITIVE_X: 1, NEGATIVE_X: 2, POSITIVE_Y: 4, NEGATIVE_Y: 8},
            Barrier: class {
                constructor(properties) {this.properties = properties; barriers.push(this);}
                connect(_name, fn) {this.hit = fn;}
                destroy() {this.destroyed = true;}
            },
        },
    };
    vm.runInNewContext(source, context);
    const extension = new context.TestExtension();
    extension.enable();
    return {extension, barriers, context, seat, backend, watch, handlers, timers, display, emitted, advance(ms) {
        now += ms * 1000;
        for (const [id, timer] of timers) {
            if (timer.due > now) continue;
            if (timer.fn() === context.GLib.SOURCE_REMOVE) timers.delete(id);
            else timer.due = now + timer.interval * 1000;
        }
    }};
}

const prepare = (edge = 'left', extra = {}) => ({command: 'prepare', token: 7, edge, start: 0, end: 1000000, position: 500000, ...extra});

for (const [edge, expectedY, direction] of [['top', 3, 4], ['bottom', 1076, 8]]) {
    const d = desktop();
    const result = await d.extension._request(prepare(edge));
    assert.equal(result.position.x, 960);
    assert.equal(result.position.y, expectedY);
    assert.equal(d.barriers[0].properties.directions, direction);
    d.barriers[0].hit(d.barriers[0], {x: 480, y: edge === 'top' ? 0 : 1080});
    assert.equal((await d.extension._request({command: 'poll', token: 7})).position, 250000);
    d.extension.disable();
}
{
    // Shells whose stage has no context fall back to Clutter.get_default_backend().
    const d = desktop();
    delete d.context.global.stage.get_context;
    d.context.Clutter.get_default_backend = () => d.backend;
    assert.equal((await d.extension._request(prepare())).status, 'prepared');
    d.extension.disable();
}
{
    const d = desktop();
    await assert.rejects(d.extension._request(prepare('left', {token: Number.MAX_SAFE_INTEGER + 1})), /Invalid handoff token/);
    assert.equal((await d.extension._request(prepare('left', {token: Number.MAX_SAFE_INTEGER}))).status, 'prepared');
    d.extension.disable();
}

{
    const d = desktop();
    const result = await d.extension._request(prepare());
    assert.equal(result.position.x, 3);
    assert.equal(result.position.y, 540);
    assert.equal(d.barriers[0].properties.directions, 1, 'left edge permits inward movement');
    const poll = d.extension._request({command: 'poll', token: 7});
    let settled = false;
    poll.then(() => {settled = true;});
    d.advance(199);
    await new Promise(setImmediate);
    assert.equal(settled, false, 'an active poll waits for its hold duration');
    d.advance(1);
    assert.equal((await poll).status, 'active');
    assert.equal(d.timers.size, 1, 'only the lease timer remains after the hold');
    d.barriers[0].hit(d.barriers[0], {x: 0, y: 270});
    assert.equal((await d.extension._request({command: 'poll', token: 7})).position, 250000);
    await assert.rejects(d.extension._request({command: 'finish', token: 8}), /expired|ended/);
    assert.equal((await d.extension._request({command: 'poll', token: 7})).status, 'returned', 'a stale token cannot clear the active lease');
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    const poll = d.extension._request({command: 'poll', token: 7});
    assert.equal(d.timers.size, 2);
    d.barriers[0].hit(d.barriers[0], {x: 0, y: 270});
    assert.equal((await poll).position, 250000, 'a pending poll returns the barrier position without advancing time');
    assert.equal(d.timers.size, 1, 'the barrier hit removes the hold timer');
    assert.equal(d.extension._lease.polls.size, 0);
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    const poll = d.extension._request({command: 'poll', token: 7});
    const rejected = assert.rejects(poll, /expired|ended/);
    // A delayed main loop can observe lease expiry before the hold callback.
    d.advance(2001);
    await rejected;
    assert.equal(d.timers.size, 1, 'lease expiry removes the pending hold timer');
    assert.ok(d.barriers.every(b => b.destroyed));
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare('right'));
    assert.equal(d.barriers[0].properties.x1, 1920);
    assert.equal(d.barriers[0].properties.directions, 2, 'right edge permits negative X');
    const poll = d.extension._request({command: 'poll', token: 7});
    const rejected = assert.rejects(poll, /expired|ended/);
    const result = await d.extension._request({command: 'finish', token: 7});
    await rejected;
    assert.equal(d.timers.size, 1, 'finish removes the pending hold timer');
    assert.equal(result.status, 'finished');
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request({command: 'poll', token: 7}), /expired|ended/);
    d.extension.disable();
}
{
    const d = desktop([{x: -200, y: -100, width: 200, height: 100}, {x: -200, y: 100, width: 200, height: 100}]);
    await assert.rejects(d.extension._request(prepare('left', {start: 366667, end: 600000})), /No GNOME monitor/);
    assert.equal(d.barriers.length, 0, 'a range inside the gap creates no barriers');
    const result = await d.extension._request(prepare('left', {position: 100000}));
    assert.deepEqual({...result.position}, {x: -197, y: -70});
    assert.equal(d.barriers.length, 2);
    await d.extension._request({command: 'finish', token: 7});
    const gap = await d.extension._request(prepare('left'));
    assert.deepEqual({...gap.position}, {x: -197, y: 100}, 'an entry into the gap moves to the nearest monitor');
    d.extension.disable();
}
{
    // 1920x1080 left of 2560x1440 with the Mac on the left: the bottom quarter of that edge has no monitor.
    const d = desktop([{x: 0, y: 0, width: 1920, height: 1080}, {x: 1920, y: 0, width: 2560, height: 1440}]);
    const result = await d.extension._request(prepare('left', {position: 900000}));
    assert.deepEqual({...result.position}, {x: 3, y: 1079});
    assert.deepEqual(d.barriers.map(b => [b.properties.y1, b.properties.y2]), [[0, 1080]]);
    d.extension.disable();
}
for (const [range, y] of [[{start: 185185, position: 185185}, 200], [{end: 500000, position: 500000}, 539]]) {
    // The first and last pixel of a partial range round inside it, not one pixel out.
    const d = desktop();
    assert.equal((await d.extension._request(prepare('left', range))).position.y, y);
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    d.advance(2001);
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request({command: 'poll', token: 7}), /expired|ended/);
    await d.extension._request(prepare('top', {token: 8}));
    d.context.Main.layoutManager.monitors.length = 0;
    d.handlers.get('monitors-changed')();
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request(prepare()), /no active monitors/);
    d.extension.disable();
}
{
    // Mutter can apply a warp a few main-loop turns late on a slow frame.
    const d = desktop();
    const warp = d.seat.warp_pointer;
    const idle = d.context.GLib.idle_add;
    let turns = 0, pending;
    d.seat.warp_pointer = (x, y) => {pending = () => warp(x, y);};
    d.context.GLib.idle_add = (priority, fn) => {
        d.advance(10);
        if (++turns === 3) pending();
        return idle(priority, fn);
    };
    assert.equal((await d.extension._request(prepare())).status, 'prepared');
    assert.equal(turns, 3);
    d.extension.disable();
}
{
    const d = desktop();
    const idle = d.context.GLib.idle_add;
    d.context.GLib.idle_add = (priority, fn) => {d.advance(10); return idle(priority, fn);};
    d.seat.warp_pointer = () => {};
    await assert.rejects(d.extension._request(prepare()), /did not place/);
    assert.ok(d.barriers.every(b => b.destroyed));
    assert.equal(d.extension._lease, null);
    d.extension.disable();
}
{
    const d = desktop();
    await d.extension._request(prepare());
    d.handlers.get('monitors-changed')();
    assert.ok(d.barriers.every(b => b.destroyed));
    await assert.rejects(d.extension._request({command: 'poll', token: 7}), /expired|ended/);
    d.extension.disable();
}
{
    const d = desktop();
    const idle = [];
    d.context.GLib.idle_add = (_priority, fn) => {idle.push(fn); return idle.length;};
    const old = d.extension._request(prepare());
    d.handlers.get('monitors-changed')();
    const replacement = d.extension._request(prepare('right', {token: 8}));
    idle[0]();
    await assert.rejects(old, /changed/);
    assert.equal(d.extension._lease.token, 8, 'cancelled entry cannot clear a newer handoff');
    idle[1]();
    assert.equal((await replacement).status, 'prepared');
    d.extension.disable();
}
{
    // Other session programs must not read the pointer, move it or hold the lease.
    const d = desktop();
    const replies = [];
    const call = (sender, request) => d.extension.CallAsync([JSON.stringify(request)], {
        get_sender: () => sender,
        return_dbus_error: name => replies.push(name),
        return_value: variant => replies.push(JSON.parse(variant.value[0]).status),
    });
    assert.equal(d.watch.name, 'io.zflow.Desktop');
    await call(':1.99', prepare());
    await call(':1.99', {command: 'snapshot'});
    assert.equal(d.extension._lease, null);
    assert.equal(d.barriers.length, 0);
    await call(':1.7', {command: 'snapshot', api: 2});
    d.watch.vanished();
    await call(':1.7', {command: 'snapshot'});
    const denied = 'org.freedesktop.DBus.Error.AccessDenied';
    assert.deepEqual(replies, [denied, denied, 'snapshot', denied]);
    d.extension.disable();
    assert.equal(d.watch.removed, 2);
}
{
    // Terminal focus reaches only the agent: once when it appears, then on each change.
    const d = desktop();
    const app = categories => ({get_app_info: () => categories === null ? null : {get_categories: () => categories}});
    const terminal = {app: app('GNOME;GTK;System;TerminalEmulator;')};
    const focus = win => {
        d.display.focus_window = win;
        d.display.handlers.get('notify::focus-window')();
    };
    const sent = () => d.emitted.splice(0).map(([to, path, bus, name, variant]) => {
        assert.deepEqual([path, bus, name], ['/org/gnome/Shell/Extensions/Zflow', 'org.gnome.Shell.Extensions.Zflow', 'FocusChanged']);
        return [to, variant.value[0]];
    });
    assert.deepEqual(sent(), [[':1.7', false]]);
    for (const win of [terminal, {app: app('System;TerminalEmulator;')}, {app: app('Development;IDE;TerminalEmulators;')}, terminal,
        {app: app(null)}, terminal, {app: null}, terminal, null])
        focus(win);
    assert.deepEqual(sent(), [[':1.7', true], [':1.7', false], [':1.7', true], [':1.7', false], [':1.7', true], [':1.7', false], [':1.7', true], [':1.7', false]],
        'only a TerminalEmulator category counts, and only changes are sent');
    focus(terminal);
    sent();
    d.context.Main.layoutManager.monitors.length = 0;
    assert.deepEqual({...await d.extension._request({command: 'focus'})}, {status: 'focus', terminal: true}, 'focus needs no monitors');
    const replies = [];
    const call = sender => d.extension.CallAsync([JSON.stringify({command: 'focus', api: 2})], {
        get_sender: () => sender,
        return_dbus_error: name => replies.push(name),
        return_value: variant => replies.push(JSON.parse(variant.value[0]).terminal),
    });
    await call(':1.99');
    await call(':1.7');
    assert.deepEqual(replies, ['org.freedesktop.DBus.Error.AccessDenied', true]);
    d.watch.vanished();
    focus(null);
    focus(terminal);
    assert.deepEqual(sent(), [], 'nothing is sent without an agent');
    d.watch.appeared(null, 'io.zflow.Desktop', ':1.8');
    assert.deepEqual(sent(), [[':1.8', true]]);
    d.extension.disable();
    assert.equal(d.display.disconnected, 3);
}
{
    // The extension and the agent update separately, so a mismatch names the older one.
    const d = desktop();
    const replies = [];
    const call = request => d.extension.CallAsync([JSON.stringify(request)], {
        get_sender: () => ':1.7',
        return_value: variant => replies.push(JSON.parse(variant.value[0])),
    });
    await call({command: 'snapshot', api: 3});
    await call({command: 'snapshot', api: 1});
    await call({command: 'snapshot'});
    await call({command: 'snapshot', api: 2});
    assert.match(replies[0].reason, /^Update zflow: its GNOME extension is older/);
    assert.match(replies[1].reason, /^Update zflow: the app is older/);
    assert.match(replies[2].reason, /^Update zflow: the app is older/, 'agents from before API levels speak API 1');
    assert.equal(replies[3].status, 'snapshot');
    d.extension.disable();
}
{
    // disable() removes the idle source of an entry still waiting for the cursor.
    const d = desktop();
    const idle = [];
    const removed = [];
    d.context.GLib.idle_add = (_priority, fn) => {idle.push(fn); return 100 + idle.length;};
    d.context.GLib.Source.remove = id => {removed.push(id); d.timers.delete(id);};
    d.extension._request(prepare());
    await new Promise(setImmediate);
    assert.equal(idle.length, 1);
    d.extension.disable();
    assert.ok(removed.includes(101));
}
{
    // This computer's own edges: barriers that report each push once.
    const d = desktop([{x: 0, y: 0, width: 1920, height: 1080}, {x: 1920, y: 0, width: 1280, height: 1024}]);
    const tracker = {count: 0, inhibit_cursor_visibility() {this.count++;}, uninhibit_cursor_visibility() {this.count--;}};
    d.context.global.backend.get_cursor_tracker = () => tracker;
    await assert.rejects(d.extension._request({command: 'edges', edges: [{edge: 'right', start: 5, end: 5}]}), /Invalid outbound edges/);
    await assert.rejects(d.extension._request({command: 'edges', edges: [{edge: 'middle', start: 0, end: 5}]}), /Invalid outbound edges/);
    assert.equal((await d.extension._request({command: 'edges', edges: [{edge: 'right', start: 0, end: 1000000}]})).status, 'finished');
    assert.equal(d.barriers.length, 1, 'only the monitor on the outer right edge');
    const barrier = d.barriers[0];
    assert.equal(barrier.properties.x1, 3200);
    assert.equal(barrier.properties.y2, 1024);
    assert.equal(barrier.properties.directions, 2, 'the pointer may come back in');
    const hits = () => d.emitted.filter(args => args[3] === 'EdgeHit').map(args => [args[0], ...args[4].value]);
    barrier.hit(barrier, {x: 3200, y: 540, event_id: 1});
    barrier.hit(barrier, {x: 3200, y: 541, event_id: 1});
    assert.deepEqual(hits(), [[':1.7', 'right', 500000]], 'one report per push, to the agent only');
    barrier.hit(barrier, {x: 3200, y: 0, event_id: 2});
    assert.deepEqual(hits().at(-1), [':1.7', 'right', 0]);
    // While another computer controls this one, its return barrier answers.
    await d.extension._request(prepare('right'));
    barrier.hit(barrier, {x: 3200, y: 100, event_id: 3});
    assert.equal(hits().length, 2);
    await d.extension._request({command: 'finish', token: 7});
    d.handlers.get('monitors-changed')();
    assert.ok(barrier.destroyed && !d.barriers.at(-1).destroyed, 'a monitor change places the edges again');
    // Sending hides the pointer once, and shows it again after.
    await d.extension._request({command: 'sending', active: true});
    await d.extension._request({command: 'sending', active: true});
    assert.equal(tracker.count, 1);
    await d.extension._request({command: 'sending', active: false});
    assert.equal(tracker.count, 0);
    await assert.rejects(d.extension._request({command: 'sending', active: 'yes'}), /Invalid sending state/);
    assert.equal((await d.extension._request({command: 'warp', position: {x: 2000, y: 900}})).status, 'finished');
    assert.deepEqual({...d.extension._snapshot().position}, {x: 2000, y: 900});
    await assert.rejects(d.extension._request({command: 'warp', position: {x: 2000, y: 1050}}), /outside the monitors/);
    await d.extension._request({command: 'sending', active: true});
    d.extension.disable();
    assert.equal(tracker.count, 0, 'disable shows the pointer again');
    assert.ok(d.barriers.every(b => b.destroyed));
}
console.log('GNOME desktop entry, return, geometry, stale requests, lease, monitor loss, placement, caller, terminal focus, outbound edges, pointer hiding, warp, API and cleanup checks passed');
