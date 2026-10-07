import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// Execute the shipped extension with deterministic compositor operations.
const source = fs.readFileSync(new URL('../packaging/gnome-extension/extension.js', import.meta.url), 'utf8')
    .replace(/^import .*;\r?\n/gm, '').replace('export default class ', 'class ') + '\nglobalThis.TestExtension = ZflowExtension;';

function desktop(monitors = [{x: 0, y: 0, width: 1920, height: 1080}], monitorData = {}) {
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
    const notices = [];
    const display = {focus_window: null, handlers: new Map(),
        connect(name, fn) {this.handlers.set(name, fn); return 3;}, disconnect(id) {this.disconnected = id;}};
    // St.Clipboard as GNOME Shell has it: the app that copied offers some
    // types and hands the bytes over later, or never when `manual` is set.
    // St frees the bytes as soon as the callback returns.
    const clipboard = {offered: {}, manual: false, replies: [], asked: 0, written: [],
        get_mimetypes(type) {assert.equal(type, 1); this.asked++; return Object.keys(this.offered);},
        get_content(type, mimetype, callback) {
            assert.equal(type, 1);
            const data = this.offered[mimetype];
            const reply = () => {
                const bytes = data === undefined ? null : new context.GLib.Bytes(data);
                callback(this, bytes);
                bytes?.free();
            };
            if (this.manual) this.replies.push(reply);
            else queueMicrotask(reply);
        },
        set_text(type, text) {assert.equal(type, 1); this.written.push(['text', text]);},
        set_content(type, mimetype, bytes) {assert.equal(type, 1); this.written.push([mimetype, Array.from(bytes.toArray())]);},
    };
    // GNOME 51 reaches the backend only through the stage context.
    const context = {
        API: 5,
        Extension: class {},
        Indicator: class { destroy() {} },
        TextDecoder,
        global: {backend: {capabilities: 1}, stage: {get_context: () => ({get_backend: () => backend})},
            display, get_pointer: () => [pointer.x, pointer.y]},
        Main: {layoutManager: {monitors, connect(name, fn) {handlers.set(name, fn); return 1;}, disconnect() {}},
            notify(title, body) {notices.push([title, body]);}},
        St: {Clipboard: {get_default: () => clipboard}, ClipboardType: {PRIMARY: 0, CLIPBOARD: 1}},
        GLib: {
            PRIORITY_DEFAULT: 0, PRIORITY_DEFAULT_IDLE: 0, SOURCE_CONTINUE: true, SOURCE_REMOVE: false,
            get_monotonic_time: () => now,
            timeout_add(_priority, interval, fn) {const id = next++; timers.set(id, {fn, interval, due: now + interval * 1000}); return id;},
            idle_add(_priority, fn) {queueMicrotask(fn); return next++;},
            Source: {remove(id) {timers.delete(id);}},
            ChecksumType: {SHA256: 1},
            compute_checksum_for_string(_kind, text) {return Buffer.from(text).toString('hex').slice(0,64);},
            Variant: class { constructor(_type, value) {this.value = value;} },
            Bytes: class {
                constructor(data) {this.data = Uint8Array.from(data);}
                free() {this.data = null;}
                get_size() {assert.ok(this.data, 'bytes used after St freed them'); return this.data.length;}
                toArray() {assert.ok(this.data, 'bytes used after St freed them'); return this.data;}
            },
        },
        Gio: {Cancellable: class {cancel() {}}, DBusCallFlags: {NONE: 0}, DBus: {session: {
            emit_signal(...args) {emitted.push(args);},
            call(_dest,_path,_iface,method,_args,_type,_flags,_timeout,_cancel,done) {done(this, {method});},
            call_finish({method}) {
                if (method === 'GetResources') {
                    if (monitorData.resourcesError) throw new Error('Unavailable');
                    return {deep_unpack: () => [monitorData.resourceSerial ?? 1, [],
                        monitors.map((m,i) => [i,0,i,[],String(i),[],[],
                            {vendor:'vendor',product:'panel',serial:'serial',...monitorData.resources}])]};
                }
                return {deep_unpack: () => [1,
                    monitors.map((m,i) => [[String(i),'vendor','panel','serial'],[],{'display-name': 'Display '+i,...monitorData.current}]),
                    monitors.map((m,i) => [m.x,m.y,1,monitorData.transform ?? 0,i===0,[[String(i),'vendor','panel','serial']],{}]),{}]};
            }
        }}, DBusExportedObject: {wrapJSObject() {return {export() {}, unexport() {}};}},
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
                connect(name, fn) {this[name] = fn;}
                destroy() {this.destroyed = true;}
            },
        },
    };
    vm.runInNewContext(source, context);
    const extension = new context.TestExtension();
    extension.enable();
    return {extension, barriers, context, seat, backend, watch, handlers, timers, display, emitted, clipboard, notices, advance(ms) {
        now += ms * 1000;
        for (const [id, timer] of timers) {
            if (timer.due > now) continue;
            if (timer.fn() === context.GLib.SOURCE_REMOVE) timers.delete(id);
            else timer.due = now + timer.interval * 1000;
        }
    }};
}

// By default the only way off is back through the edge the pointer came in at.
const prepare = (edge = 'left', extra = {}) => {
    const {monitor, start = 0, end = 1000000} = extra;
    return {command: 'prepare', token: 7, edge, start, end, position: 500000,
        exits: [{edge, start, end, ...(monitor === undefined ? {} : {monitor})}], ...extra};
};

{
    const d = desktop(undefined, {resources: {'width-mm': 530, 'height-mm': 300}});
    const screen = (await d.extension._request({command: 'snapshot'})).geometry.displays[0];
    assert.equal(screen.width_mm, 530, 'Mutter GetResources supplies dimensions absent from GetCurrentState');
    assert.equal(screen.height_mm, 300);
    d.extension.disable();
}
{
    const d = desktop(undefined, {resources: {'width-mm': 530, 'height-mm': 300}, transform: 1});
    const screen = (await d.extension._request({command: 'snapshot'})).geometry.displays[0];
    assert.equal(screen.width_mm, 300, 'rotated monitor dimensions follow its orientation');
    assert.equal(screen.height_mm, 530);
    d.extension.disable();
}
for (const monitorData of [{resources: {serial: 'replaced', 'width-mm': 530, 'height-mm': 300}}, {resourcesError: true}]) {
    const d = desktop(undefined, monitorData);
    const screen = (await d.extension._request({command: 'snapshot'})).geometry.displays[0];
    assert.equal(screen.width_mm, 0, 'missing or different connector identity never supplies dimensions');
    assert.equal(screen.height_mm, 0);
    d.extension.disable();
}
{
    const d = desktop(undefined, {resourceSerial: 2, resources: {'width-mm': 530, 'height-mm': 300}});
    await assert.rejects(d.extension._request({command: 'snapshot'}), /Waiting for GNOME monitor information/);
    d.extension.disable();
}

{
    // An exposed monitor edge can lie inside the whole desktop's bounding box.
    const d = desktop([{x: -1920, y: 0, width: 1920, height: 1080}, {x: 0, y: 0, width: 2560, height: 1440}]);
    const snapshot = await d.extension._request({command: 'snapshot'});
    assert.equal(snapshot.geometry.displays.length, 2);
    const monitor = snapshot.geometry.displays[0].id;
    const result = await d.extension._request(prepare('bottom', {monitor}));
    assert.deepEqual({...result.position}, {x: -960, y: 1076});
    const barrier = d.barriers.find(b => !b.destroyed);
    assert.equal(barrier.properties.y1, 1080, 'selected monitor bottom');
    barrier.hit(barrier, {x: -480, y: 1080});
    assert.equal((await d.extension._request({command: 'poll', token: 7})).position, 750000);
    await d.extension._request({command: 'finish', token: 7});
    await assert.rejects(d.extension._request(prepare('bottom', {monitor: 'disconnected'})), /disconnected/);
    assert.equal(d.extension._lease, null, 'unknown monitors never acquire input');
    await d.extension._request({command: 'edges', edges: [{monitor, edge: 'bottom', start: 0, end: 1000000}]});
    const outbound = d.barriers.findLast(b => !b.destroyed);
    outbound.hit(outbound, {x: -960, y: 1080, event_id: 10});
    const hit = d.emitted.findLast(args => args[3] === 'EdgeHit')[4].value;
    assert.deepEqual(Array.from(hit), [monitor, 'bottom', 500000]);
    d.context.Main.layoutManager.monitors.splice(0,1);
    d.handlers.get('monitors-changed')();
    assert.ok(outbound.destroyed, 'unplugging removes the old barrier');
    d.extension.disable();
}

{
    // An arranged remote edge takes precedence over the adjacent local screen.
    // This matches the XPS's native GNOME topology and barrier validation.
    const d = desktop([{x: 0, y: 0, width: 1536, height: 960}, {x: 1536, y: 0, width: 1920, height: 1080}]);
    const {displays} = (await d.extension._request({command: 'snapshot'})).geometry;
    for (const [index, edge, entryX] of [[0, 'right', 1532], [1, 'left', 1539]]) {
        const monitor = displays[index].id;
        const prepared = await d.extension._request(prepare(edge, {monitor}));
        assert.equal(prepared.position.x, entryX);
        const returning = d.barriers.findLast(b => !b.destroyed);
        assert.equal(returning.properties.x1, 1536, 'return barrier stays on the selected internal edge');
        returning.hit(returning, {x: 1536, y: prepared.position.y});
        assert.equal((await d.extension._request({command: 'poll', token: 7})).position, 500000);
        await d.extension._request({command: 'finish', token: 7});
        await d.extension._request({command: 'edges', edges: [{monitor, edge, start: 0, end: 1000000}]});
        const outbound = d.barriers.findLast(b => !b.destroyed);
        assert.equal(outbound.properties.x1, 1536, 'a local neighbor does not remove the outbound barrier');
        outbound.hit(outbound, {x: 1536, y: prepared.position.y, event_id: index + 30});
        const hit = d.emitted.findLast(args => args[3] === 'EdgeHit')[4].value;
        assert.deepEqual(Array.from(hit), [monitor, edge, 500000]);
        await d.extension._request({command: 'edges', edges: []});
    }
    d.extension.disable();
}

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
    await assert.rejects(d.extension._request({command: 'finish', token: 8}), /expired|ended/);
    assert.deepEqual({...await d.extension._request({command: 'poll', token: 7})}, {status: 'exited', exit: 0, position: 250000},
        'a stale token cannot clear the active lease');
    // The sending computer keeps the pointer here, say because a key is
    // held: it polls again, and the same exit reports on the next push.
    const kept = d.extension._request({command: 'poll', token: 7});
    let answered = false;
    kept.then(() => {answered = true;});
    await new Promise(setImmediate);
    assert.equal(answered, false, 'a report is answered once');
    d.barriers[0].hit(d.barriers[0], {x: 0, y: 540});
    assert.deepEqual({...await kept}, {status: 'exited', exit: 0, position: 500000});
    d.extension.disable();
}
{
    // Every exit gets its barriers: home through the left edge, on to
    // another computer from the right half of the bottom edge.
    const d = desktop([{x: 0, y: 0, width: 1920, height: 1080}, {x: 1920, y: 0, width: 1920, height: 1080}]);
    const {displays} = (await d.extension._request({command: 'snapshot'})).geometry;
    const [first, second] = displays.map(display => display.id);
    const exits = [{edge: 'left', start: 0, end: 1000000, monitor: first},
        {edge: 'bottom', start: 500000, end: 1000000, monitor: second},
        {edge: 'top', start: 0, end: 1000000, monitor: 'unplugged'}];
    assert.equal((await d.extension._request(prepare('left', {monitor: first, exits}))).status, 'prepared');
    const [home, onward, ...rest] = d.barriers.filter(b => !b.destroyed);
    assert.equal(rest.length, 0, 'an exit on a monitor that is gone has no barrier');
    assert.deepEqual([home.properties.x1, home.properties.y1, home.properties.y2], [0, 0, 1080]);
    assert.deepEqual([onward.properties.x1, onward.properties.x2, onward.properties.y1], [2880, 3840, 1080]);
    onward.hit(onward, {x: 3360, y: 1080});
    home.hit(home, {x: 0, y: 10});
    assert.deepEqual({...await d.extension._request({command: 'poll', token: 7})}, {status: 'exited', exit: 1, position: 750000},
        'the first exit the pointer reaches is the one reported');
    await d.extension._request({command: 'finish', token: 7});
    assert.ok(d.barriers.every(b => b.destroyed));
    for (const invalid of [undefined, [{edge: 'middle', start: 0, end: 1}], [{edge: 'left', start: 5, end: 5}],
        [{edge: 'left', start: 0, end: 1, monitor: ''}], Array(65).fill({edge: 'left', start: 0, end: 1})])
        await assert.rejects(d.extension._request({...prepare(), exits: invalid}), /Invalid crossing exits/);
    assert.equal(d.extension._lease, null);
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
    await assert.rejects(d.extension._request(prepare()), /did not place the cursor at the requested entry: asked for 3,540 and it is at 100,100/);
    assert.ok(d.barriers.every(b => b.destroyed));
    assert.equal(d.extension._lease, null);
    d.extension.disable();
}
{
    // Mutter runs a warp through the pointer barriers on its input thread, like
    // any motion (meta-seat-impl.c warp_pointer_in_impl, constrain_coordinates).
    // A barrier the pointer rests on that blocks a direction of the warp pins
    // the pointer to it (meta-border.c meta_border_is_blocking_directions,
    // meta-barrier-native.c clamp_to_barrier).
    const d = desktop();
    const warp = d.seat.warp_pointer;
    d.seat.warp_pointer = (x, y) => queueMicrotask(() => {
        const [px, py] = d.context.global.get_pointer();
        const motion = (x > px ? 1 : x < px ? 2 : 0) | (y > py ? 4 : y < py ? 8 : 0);
        for (const barrier of d.barriers.filter(b => !b.destroyed)) {
            const {x1, x2, y1, y2, directions} = barrier.properties;
            const vertical = x1 === x2;
            const on = vertical ? px === x1 && py >= Math.min(y1, y2) && py <= Math.max(y1, y2)
                : py === y1 && px >= Math.min(x1, x2) && px <= Math.max(x1, x2);
            if (on && motion & (vertical ? 3 : 12) && motion & ~directions) {
                if (vertical) x = x1;
                else y = y1;
            }
        }
        warp(x, y);
    });
    const idle = d.context.GLib.idle_add;
    d.context.GLib.idle_add = (priority, fn) => {d.advance(10); return idle(priority, fn);};
    // As in the live test: the pointer came back from the Mac and rests on
    // this computer's own left edge, where its barrier to the Mac is.
    await d.extension._request({command: 'edges', edges: [{edge: 'left', start: 0, end: 1000000}]});
    warp(0, 270);
    const result = await d.extension._request(prepare());
    assert.deepEqual({...result.position}, {x: 3, y: 540}, 'the entry is not pinned to the edge');
    assert.ok(d.barriers.filter(b => !b.destroyed).every(b => b.properties.y2 === 1080),
        'only the return barrier is up while the Mac controls this computer');
    await d.extension._request({command: 'finish', token: 7});
    const own = d.barriers.filter(b => !b.destroyed);
    assert.equal(own.length, 1, 'its own barrier comes back');
    assert.deepEqual([own[0].properties.y1, own[0].properties.y2, own[0].properties.directions], [8, 1072, 1]);
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
    await call(':1.7', {command: 'snapshot', api: 5});
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
    const call = sender => d.extension.CallAsync([JSON.stringify({command: 'focus', api: 5})], {
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
    await call({command: 'snapshot', api: 6});
    await call({command: 'snapshot', api: 1});
    await call({command: 'snapshot'});
    await call({command: 'snapshot', api: 5});
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
    assert.equal(barrier.properties.y1, 8, 'the top corner of the desktop stays dead');
    assert.equal(barrier.properties.y2, 1024, 'this monitor ends above the desktop corner');
    assert.equal(barrier.properties.directions, 2, 'the pointer may come back in');
    const hits = () => d.emitted.filter(args => args[3] === 'EdgeHit').map(args => [args[0], ...args[4].value]);
    barrier.hit(barrier, {x: 3200, y: 540, event_id: 1});
    barrier.hit(barrier, {x: 3200, y: 541, event_id: 1});
    assert.deepEqual(hits(), [[':1.7', '', 'right', 500000]], 'one report per push, to the agent only');
    barrier.hit(barrier, {x: 3200, y: 0, event_id: 2});
    assert.deepEqual(hits().at(-1), [':1.7', '', 'right', 0]);
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
    const placed = d.barriers.length;
    await d.extension._request({command: 'edges', edges: [{edge: 'right', start: 0, end: 1000000}]});
    assert.equal(d.barriers.length, placed, 'the same edges keep their barriers and the push in progress');
    await d.extension._request({command: 'sending', active: true});
    d.watch.vanished();
    assert.equal(tracker.count, 0, 'without the agent the pointer shows again');
    assert.ok(d.barriers.every(b => b.destroyed), 'and no barrier is left');
    d.watch.appeared(null, 'io.zflow.Desktop', ':1.7');
    await d.extension._request({command: 'sending', active: true});
    d.extension.disable();
    assert.equal(tracker.count, 0, 'disable shows the pointer again');
    assert.ok(d.barriers.every(b => b.destroyed));
}
{
    // With a pause, a push crosses only after the pointer rests against the edge.
    const d = desktop();
    await d.extension._request({command: 'edges', edges: [{edge: 'left', start: 0, end: 1000000}], pause_ms: 250});
    await assert.rejects(d.extension._request({command: 'edges', edges: [], pause_ms: 5000}), /Invalid outbound edges/);
    const barrier = d.barriers[0];
    const hits = () => d.emitted.filter(args => args[3] === 'EdgeHit').map(args => [...args[4].value]);
    barrier.hit(barrier, {x: 0, y: 270, event_id: 1});
    d.advance(200);
    barrier.hit(barrier, {x: 0, y: 540, event_id: 1});
    assert.deepEqual(hits(), [], 'still resting');
    d.advance(50);
    assert.deepEqual(hits(), [['', 'left', 500000]], 'crosses where the pointer rests after the pause');
    barrier.hit(barrier, {x: 0, y: 100, event_id: 2});
    barrier.left();
    d.advance(300);
    assert.equal(hits().length, 1, 'leaving the edge cancels the crossing');
    barrier.hit(barrier, {x: 0, y: 100, event_id: 3});
    d.extension.disable();
    d.advance(300);
    assert.equal(hits().length, 1, 'disable drops a waiting push');
    assert.equal(d.timers.size, 0);
}
// src/clipboard.rs MAX_CLIP_BYTES.
const MAX_CLIP_BYTES = 3 * 1024 * 1024;
const PNG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1];
const utf8 = text => Array.from(new TextEncoder().encode(text));
// The clipboard over D-Bus: what each call returned, as the agent would see it.
function clipboardCalls(d) {
    const replies = [];
    const invocation = sender => ({
        get_sender: () => sender,
        return_dbus_error: (error, message) => replies.push({error, message}),
        return_value: variant => replies.push(variant === null ? null : {kind: variant.value[0], data: variant.value[1]}),
    });
    const small = reply => reply?.data ? {...reply, data: Array.from(reply.data)} : reply;
    return {
        async read(sender = ':1.7') {
            await d.extension.ReadClipboardAsync([], invocation(sender));
            return small(replies.pop());
        },
        async write(kind, data, sender = ':1.7') {
            await d.extension.WriteClipboardAsync([kind, Uint8Array.from(data)], invocation(sender));
            return replies.pop();
        },
        replies,
    };
}
{
    // One kind per clip: text if there is any, else a PNG; never other types.
    const d = desktop();
    const {read} = clipboardCalls(d);
    d.clipboard.offered = {'text/html': utf8('<b>hé</b>'), 'image/png': PNG, 'text/plain;charset=utf-8': utf8('hé')};
    assert.deepEqual(await read(), {kind: 'text', data: utf8('hé')});
    d.clipboard.offered = {'image/png': PNG, 'text/plain': utf8('plain')};
    assert.deepEqual(await read(), {kind: 'text', data: utf8('plain')});
    d.clipboard.offered = {'image/png': PNG, 'image/jpeg': [0xff, 0xd8]};
    assert.deepEqual(await read(), {kind: 'png', data: PNG});
    for (const offered of [{}, {'text/html': utf8('<b>hé</b>')}, {'text/plain;charset=utf-8': []}, {'x-special/gnome-copied-files': utf8('copy')}]) {
        d.clipboard.offered = offered;
        assert.deepEqual(await read(), {kind: 'empty', data: []}, JSON.stringify(Object.keys(offered)));
    }
    // An app that offers a type and then fails to hand it over leaves nothing to send.
    d.clipboard.offered = {'image/png': undefined};
    assert.deepEqual(await read(), {kind: 'empty', data: []});
    // The limit is the same number as the service's; over it, only the size leaves.
    d.clipboard.offered = {'image/png': new Uint8Array(MAX_CLIP_BYTES + 1)};
    assert.deepEqual(await read(), {kind: `too_large:${MAX_CLIP_BYTES + 1}`, data: []});
    d.extension.disable();
}
{
    // A full clip still goes whole.
    const d = desktop();
    d.clipboard.offered = {'image/png': new Uint8Array(MAX_CLIP_BYTES)};
    let reply;
    await d.extension.ReadClipboardAsync([], {get_sender: () => ':1.7', return_value: variant => {reply = variant.value;}});
    assert.equal(reply[0], 'png');
    assert.equal(reply[1].length, MAX_CLIP_BYTES);
    d.extension.disable();
}
{
    // A stuck app gets a second, then the agent hears why; a late answer is dropped.
    const d = desktop();
    const {read, replies} = clipboardCalls(d);
    d.clipboard.manual = true;
    d.clipboard.offered = {'text/plain': utf8('late')};
    const pending = read();
    await new Promise(setImmediate);
    d.advance(999);
    await new Promise(setImmediate);
    assert.equal(replies.length, 0, 'still waiting for the app');
    d.advance(1);
    const failed = await pending;
    assert.equal(failed.error, 'org.freedesktop.DBus.Error.Failed');
    assert.match(failed.message, /did not hand the clipboard over/);
    d.clipboard.replies.shift()();
    await new Promise(setImmediate);
    assert.equal(replies.length, 0, 'the late answer goes nowhere');
    // Turning the extension off ends a read that is still waiting.
    const waiting = read();
    await new Promise(setImmediate);
    d.extension.disable();
    assert.match((await waiting).message, /turned off/);
    assert.equal(d.timers.size, 0, 'no clipboard timer is left');
}
{
    // Only the agent reads or writes the clipboard.
    const d = desktop();
    const {read, write} = clipboardCalls(d);
    d.clipboard.offered = {'text/plain;charset=utf-8': utf8('secret')};
    const denied = 'org.freedesktop.DBus.Error.AccessDenied';
    assert.equal((await read(':1.99')).error, denied);
    assert.equal((await write('text', utf8('planted'), ':1.99')).error, denied);
    assert.equal(d.clipboard.asked, 0, 'a stranger never makes the clipboard be read');
    assert.deepEqual(d.clipboard.written, []);
    d.watch.vanished();
    assert.equal((await read(':1.7')).error, denied, 'nor does a caller once the agent is gone');
    assert.equal(d.clipboard.asked, 0);
    d.extension.disable();
}
{
    // Writing puts text or a PNG on the clipboard, and nothing else.
    const d = desktop();
    const {write} = clipboardCalls(d);
    assert.equal(await write('text', utf8('héllo')), null);
    assert.equal(await write('png', PNG), null);
    assert.deepEqual(d.clipboard.written, [['text', 'héllo'], ['image/png', PNG]]);
    for (const [kind, data, reason] of [['text', [0xff, 0xfe], /./], ['gif', [1], /Unknown clip kind/],
        ['text', [], /Invalid clip/], ['png', new Uint8Array(MAX_CLIP_BYTES + 1), /Invalid clip/]]) {
        const refused = await write(kind, data);
        assert.equal(refused.error, 'org.freedesktop.DBus.Error.InvalidArgs', kind);
        assert.match(refused.message, reason);
    }
    assert.equal(d.clipboard.written.length, 2);
    d.extension.disable();
}
{
    // A notice from the service shows as a GNOME notification, even without monitors.
    const d = desktop();
    d.context.Main.layoutManager.monitors.length = 0;
    const notice = 'Clipboard not shared: 5.0 MB is over the 3 MB limit';
    assert.equal((await d.extension._request({command: 'notify', message: notice})).status, 'finished');
    await d.extension._request({command: 'notify', message: 'x'.repeat(300)});
    assert.deepEqual(d.notices, [['zflow', notice], ['zflow', 'x'.repeat(256)]]);
    for (const message of [undefined, '', 7]) await assert.rejects(d.extension._request({command: 'notify', message}), /Invalid notice/);
    const replies = [];
    await d.extension.CallAsync([JSON.stringify({command: 'notify', message: 'hi', api: 5})], {
        get_sender: () => ':1.99',
        return_dbus_error: name => replies.push(name),
    });
    assert.deepEqual(replies, ['org.freedesktop.DBus.Error.AccessDenied']);
    assert.equal(d.notices.length, 2, 'only the agent shows notices');
    d.extension.disable();
}
console.log('GNOME desktop entry, return, geometry, stale requests, lease, monitor loss, placement, caller, terminal focus, outbound edges, pause at edges, pointer hiding, warp, clipboard read and write, notices, API and cleanup checks passed');
