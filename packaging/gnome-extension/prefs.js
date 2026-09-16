import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';
import {Settings} from './settings.js';

export default class ZflowPreferences extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        window.default_width = 520;
        window.default_height = 640;
        const settings = new Settings(window);
        window.add(settings.page);
        window.connect('close-request', () => { settings.destroy(); return false; });
    }
}
