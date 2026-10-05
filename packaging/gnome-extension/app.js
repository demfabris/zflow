import Adw from 'gi://Adw?version=1';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import {Settings} from './settings.js';
import {Setup} from './setup.js';
import {Updates} from './updates.js';

const flags = Gio.ApplicationFlags.ALLOW_REPLACEMENT
    | (GLib.getenv('ZFLOW_REPLACE') === '1' ? Gio.ApplicationFlags.REPLACE : Gio.ApplicationFlags.FLAGS_NONE);
const app = new Adw.Application({application_id: 'io.zflow.zflow', flags});
app.connect('activate', () => {
    if (app.active_window) { app.active_window.present(); return; }
    const window = new Adw.ApplicationWindow({application: app, title: 'zflow', icon_name: 'io.zflow.zflow', default_width: 520, default_height: 640});
    const toolbar = new Adw.ToolbarView();
    toolbar.add_top_bar(new Adw.HeaderBar());
    const settings = new Settings(window);
    const setup = new Setup(settings.client);
    const updates = new Updates(window);
    settings.page.add(updates.group);
    toolbar.add_top_bar(setup.banner);
    toolbar.content = settings.page;
    window.content = toolbar;
    window.connect('close-request', () => updates.installing);
    app.connect('shutdown', () => { setup.destroy(); settings.destroy(); updates.destroy(); });
    window.present();
});
app.run([]);
