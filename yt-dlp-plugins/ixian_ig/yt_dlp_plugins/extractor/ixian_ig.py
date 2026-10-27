"""Instagram: serve media from a reachable CDN hostname.

Instagram hands out signed media URLs on a geo-picked edge such as
`instagram.frix2-1.fna.fbcdn.net` or `scontent-hel3-1.cdninstagram.com`.
Some ISP-local edges silently drop TLS handshakes (DPI) or route to an
unreachable address, so the download stalls after a successful extraction.
The signature is not bound to the host, so the same path works on the shared
`scontent.cdninstagram.com` endpoint.

Set IXIAN_IG_CDN_HOST to another hostname, or to an empty string to disable.
"""
import os
import re
from urllib.parse import urlsplit, urlunsplit

from yt_dlp.extractor.instagram import InstagramIE as _InstagramIE

CDN_HOST = os.environ.get('IXIAN_IG_CDN_HOST', 'scontent.cdninstagram.com')
_EDGE_RE = re.compile(r'(^|\.)(?:cdninstagram\.com|fbcdn\.net)$', re.IGNORECASE)


def _rewrite(url):
    if not CDN_HOST or not isinstance(url, str):
        return url
    parts = urlsplit(url)
    if not parts.hostname or not _EDGE_RE.search(parts.hostname):
        return url
    return urlunsplit(parts._replace(netloc=CDN_HOST))


def _patch(info):
    if not isinstance(info, dict):
        return info
    for fmt in info.get('formats') or []:
        fmt['url'] = _rewrite(fmt.get('url'))
    if info.get('url'):
        info['url'] = _rewrite(info['url'])
    for entry in info.get('entries') or []:
        _patch(entry)
    return info


class InstagramIE(_InstagramIE, plugin_name='ixian'):
    def _real_extract(self, url):
        return _patch(super()._real_extract(url))
