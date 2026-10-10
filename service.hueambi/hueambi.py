# -*- coding: utf-8 -*-
"""Code commun au service et aux actions de l'add-on Hue Ambi."""
import os
import re

import xbmc
import xbmcaddon
import xbmcvfs

ID = 'service.hueambi'
PROFILE = xbmcvfs.translatePath('special://profile/addon_data/%s/' % ID)
CONF = os.path.join(PROFILE, 'hue-ambi.conf')
OLD_SETTINGS = xbmcvfs.translatePath('special://profile/addon_data/script.service.hue/settings.xml')
REQUIRED = ('bridge', 'key', 'clientkey', 'area')


def log(msg, level=xbmc.LOGINFO):
    xbmc.log('[hue-ambi] %s' % msg, level)


def addon():
    return xbmcaddon.Addon(ID)


def tr(sid):
    """Chaine traduite (resources/language/*/strings.po)."""
    return addon().getLocalizedString(sid)


def binary():
    path = os.path.join(addon().getAddonInfo('path'), 'bin', 'hue-ambi')
    if not os.access(path, os.X_OK):
        os.chmod(path, 0o755)
    return path


def env(conf=CONF):
    e = dict(os.environ)
    e['HUE_AMBI_CONF'] = conf
    return e


def values(a):
    """Reglages de l'add-on -> cles du fichier de configuration du moteur."""
    v = {k: a.getSetting(k).strip() for k in REQUIRED}
    v['fps'] = a.getSetting('fps') or '50'
    v['smoothing'] = a.getSetting('smoothing') or '0.35'
    v['saturation'] = a.getSetting('saturation') or '1.25'
    v['gain'] = a.getSetting('gain') or '1.0'
    v['sync'] = 'dc'
    return v


def write_conf(v, path=CONF):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        f.write("# genere par service.hueambi : modifier via les reglages de l'add-on\n")
        for k, val in v.items():
            f.write('%s=%s\n' % (k, val))


def read_conf(path):
    out = {}
    try:
        with open(path, encoding='utf-8') as f:
            for line in f:
                if '=' in line and not line.startswith('#'):
                    k, val = line.split('=', 1)
                    out[k.strip()] = val.strip()
    except OSError:
        pass
    return out


def import_old_credentials(a):
    """Premier lancement : reprend l'IP du pont et la cle de script.service.hue."""
    if a.getSetting('bridge') or not os.path.exists(OLD_SETTINGS):
        return
    with open(OLD_SETTINGS, encoding='utf-8') as f:
        txt = f.read()

    def get(k):
        m = re.search(r'<setting id="%s"[^>]*>([^<]*)<' % k, txt)
        return m.group(1).strip() if m else ''

    ip, user = get('bridgeIP'), get('bridgeUser')
    if ip:
        a.setSetting('bridge', ip)
    if user and not a.getSetting('key'):
        a.setSetting('key', user)
        a.setSetting('paired', tr(30108))
    log('pont %s repris de script.service.hue' % ip)
