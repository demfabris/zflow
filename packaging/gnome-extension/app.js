import Adw from 'gi://Adw?version=1';
import {Settings} from './settings.js';

const app = new Adw.Application({application_id: 'io.zflow.zflow'});
app.connect('activate', () => {
    if (app.active_window) { app.active_window.present(); return; }
    const window = new Adw.ApplicationWindow({application: app, title: 'zflow', default_width: 520, default_height: 640});
    const toolbar = new Adw.ToolbarView();
    toolbar.add_top_bar(new Adw.HeaderBar());
    const settings = new Settings(window);
    toolbar.content = settings.page;
    window.content = toolbar;
    window.connect('close-request', () => { settings.destroy(); return false; });
    window.present();
});
app.run([]);
