# -*- coding: utf-8 -*-
"""Actions des reglages : appairage du pont et choix de la zone de divertissement."""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import xbmc  # noqa: E402
import xbmcgui  # noqa: E402

from hueambi import CONF, PROFILE, addon, binary, env, log, read_conf, tr, values, write_conf  # noqa: E402

KINDS = {'screen': 30110, 'monitor': 30111, 'music': 30112, '3dspace': 30113, 'other': 30114}


def pair():
    a = addon()
    dlg = xbmcgui.Dialog()
    ip = dlg.input(tr(30100), a.getSetting('bridge').strip(), type=xbmcgui.INPUT_IPADDRESS)
    if not ip:
        return False
    tmp = os.path.join(PROFILE, 'pair.conf')
    os.makedirs(PROFILE, exist_ok=True)
    if os.path.exists(tmp):
        os.remove(tmp)
    proc = subprocess.Popen([binary(), 'pair', ip], env=env(tmp), stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    prog = xbmcgui.DialogProgress()
    prog.create('Hue Ambi', tr(30101))
    t0 = time.time()
    canceled = False
    while proc.poll() is None:
        if prog.iscanceled():
            canceled = True
            proc.kill()
            break
        left = max(0, 60 - int(time.time() - t0))
        prog.update(100 - left * 100 // 60, tr(30102) % left)
        xbmc.sleep(250)
    proc.wait()
    prog.close()
    out = proc.stdout.read().decode('utf-8', 'replace').strip()
    got = read_conf(tmp)
    if os.path.exists(tmp):
        os.remove(tmp)
    if proc.returncode != 0 or not got.get('clientkey'):
        if not canceled:
            dlg.ok('Hue Ambi', tr(30103) % out[-300:])
        return False
    a.setSetting('bridge', ip)
    a.setSetting('key', got['key'])
    a.setSetting('clientkey', got['clientkey'])
    a.setSetting('paired', tr(30104) % time.strftime('%Y-%m-%d %H:%M'))
    log('pont %s appaire' % ip)
    return True


def choose_area():
    a = addon()
    dlg = xbmcgui.Dialog()
    write_conf(values(a))
    try:
        r = subprocess.run([binary(), 'areas'], env=env(CONF), stdin=subprocess.DEVNULL,
                           capture_output=True, timeout=20)
        rows = [l.split('\t') for l in r.stdout.decode('utf-8', 'replace').splitlines() if l.count('\t') == 3]
        err = r.stderr.decode('utf-8', 'replace').strip()
    except (OSError, subprocess.TimeoutExpired) as e:
        rows, err = [], str(e)
    if not rows:
        dlg.ok('Hue Ambi', tr(30105) % err[-300:])
        return
    labels = [tr(30106) % (name, tr(KINDS[kind]) if kind in KINDS else kind, n) for _, name, kind, n in rows]
    current = a.getSetting('area')
    pre = next((i for i, row in enumerate(rows) if row[0] == current), -1)
    if pre < 0:
        pre = next((i for i, row in enumerate(rows) if row[2] == 'screen'), 0)
    i = dlg.select(tr(30005), labels, preselect=pre)
    if i < 0:
        return
    a.setSetting('area', rows[i][0])
    a.setSetting('area_name', labels[i])
    log('zone choisie : %s (%s)' % (labels[i], rows[i][0]))


if __name__ == '__main__':
    what = sys.argv[1] if len(sys.argv) > 1 else ''
    if what == 'pair':
        if pair():
            choose_area()
    elif what == 'area':
        choose_area()
    addon().openSettings()
