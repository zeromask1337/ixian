"""Connect to Meta hosts with a decoy TLS SNI.

Some networks drop the TLS ClientHello for any SNI ending in
``instagram.com`` / ``cdninstagram.com`` (DPI), while the same Meta edge
answers normally for an unrelated SNI.  Meta selects the certificate from
the SNI but routes the request by the ``Host`` header, so using a decoy SNI
still returns a ``*.instagram.com`` certificate for the requested host.

The certificate chain is verified against the system CA store and the
hostname is checked manually against the *real* host, so this only changes
what the middlebox sees, not what the client trusts.

Configure the decoy with ``IXIAN_SNI`` (default ``example.com``); set it to
an empty value or ``off`` to disable.
"""
import functools
import http.client
import os
import ssl
import threading
from urllib.parse import urlsplit

_DECOY = os.environ.get('IXIAN_SNI', 'example.com').strip()
_ENABLED = bool(_DECOY) and _DECOY.lower() not in ('off', '0', 'false', 'no')

_META_SUFFIXES = (
    '.instagram.com',
    '.cdninstagram.com',
    '.fbcdn.net',
    '.facebook.com',
    '.igsonar.com',
)
_META_HOSTS = tuple(s[1:] for s in _META_SUFFIXES)


def _is_meta(host):
    host = (host or '').lower().rstrip('.')
    return host in _META_HOSTS or host.endswith(_META_SUFFIXES)


def _match_hostname(cert, host):
    """RFC 6125 match, replacing ``ssl.match_hostname`` (removed in Python 3.13)."""
    host = host.lower().rstrip('.')
    names = [value.lower().rstrip('.') for kind, value in cert.get('subjectAltName', ()) if kind == 'DNS']
    if not names:
        names = [value.lower().rstrip('.') for rdn in cert.get('subject', ()) for kind, value in rdn if kind == 'commonName']
    for pattern in names:
        if pattern == host:
            return
        if pattern.startswith('*.') and host.count('.') == pattern.count('.') and host.split('.', 1)[1] == pattern[2:]:
            return
    raise ssl.SSLCertVerificationError(
        f"hostname '{host}' doesn't match any certificate name ({', '.join(names) or 'none'})")


_context = None
_lock = threading.Lock()


def _decoy_context(template):
    """Context with chain verification but no automatic SNI/hostname check."""
    global _context
    with _lock:
        if _context is not None:
            return _context
        if template.verify_mode == ssl.CERT_NONE:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
            context.check_hostname = False
            context.verify_mode = ssl.CERT_NONE
        else:
            # Use the system trust store (ca-certificates / certifi via SSL_CERT_FILE).
            context = ssl.create_default_context()
            context.check_hostname = False
            context.verify_mode = ssl.CERT_REQUIRED
        for attr in ('minimum_version', 'maximum_version'):
            try:
                setattr(context, attr, getattr(template, attr))
            except (AttributeError, ValueError):
                pass
        try:
            context.set_alpn_protocols(['http/1.1'])
        except NotImplementedError:
            pass
        _context = context
        return _context


class _SNIConnection(http.client.HTTPSConnection):
    """HTTPSConnection that uses the decoy SNI for Meta hosts, then verifies the real name."""

    def connect(self):
        http.client.HTTPConnection.connect(self)

        if self._tunnel_host:
            # Proxied request: the tunnel hides the ClientHello from the local network.
            self.sock = self._context.wrap_socket(self.sock, server_hostname=self._tunnel_host)
            return

        if _ENABLED and _is_meta(self.host):
            context = _decoy_context(self._context)
            self.sock = context.wrap_socket(self.sock, server_hostname=_DECOY)
            if context.verify_mode != ssl.CERT_NONE:
                _match_hostname(self.sock.getpeercert(), self.host)
        else:
            self.sock = self._context.wrap_socket(self.sock, server_hostname=self.host)


def _disable_instagram_impersonation():
    """Use the urllib handler for Instagram so the SNI hook below applies.

    curl_cffi/libcurl cannot set a different SNI than the URL host, so it
    would bypass this workaround on installs that have curl_cffi available.
    """
    try:
        from yt_dlp.extractor.instagram import InstagramIE
    except ImportError:
        return
    if not getattr(InstagramIE, '_ixian_no_impersonate', False):
        InstagramIE._can_impersonate = property(lambda self: False)
        InstagramIE._ixian_no_impersonate = True


def _prefer_urllib(handler, request):
    """Keep requests/curl_cffi from handling Meta URLs; they cannot set a decoy SNI."""
    if handler.RH_NAME in ('requests', 'curl_cffi'):
        if _is_meta(urlsplit(request.url).hostname):
            return -1000
    return 0


def _patch():
    from yt_dlp.networking._urllib import HTTPHandler, _create_http_connection
    from yt_dlp.networking.common import _RH_PREFERENCES
    if getattr(HTTPHandler, '_ixian_sni_patched', False):
        return
    _RH_PREFERENCES.add(_prefer_urllib)
    original = HTTPHandler.https_open

    def https_open(self, req):
        conn_class = self._make_conn_class(_SNIConnection, req)
        return self.do_open(
            functools.partial(_create_http_connection, conn_class, self._source_address),
            req, context=self._context)

    https_open.__doc__ = original.__doc__
    HTTPHandler.https_open = https_open
    HTTPHandler._ixian_sni_patched = True


if _ENABLED:
    _disable_instagram_impersonation()
    _patch()
