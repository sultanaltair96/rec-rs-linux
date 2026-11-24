#!/usr/bin/env python3
"""Private-bus ScreenCast + SNI watcher, using the image's GLib (no pip).

CreateSession/SelectSources succeed; Start returns a request but NEVER emits
Response. JSONL events record every request and Close, with monotonic times.
Only run on a disposable dbus-run-session bus.
"""
import ctypes as C
import json
import time

G = C.CDLL('libgio-2.0.so.0')
L = C.CDLL('libglib-2.0.so.0')
P = C.c_void_p
S = C.c_char_p
U = C.c_uint

def bind(lib, name, result, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = result, args
    return f

parse = bind(L, 'g_variant_parse', P, P, S, P, P, P)
child = bind(L, 'g_variant_get_child_value', P, P, C.c_size_t)
string = bind(L, 'g_variant_get_string', S, P, P)
lookup = bind(L, 'g_variant_lookup_value', P, P, S, P)
reply = bind(G, 'g_dbus_method_invocation_return_value', None, P, P)
emit = bind(G, 'g_dbus_connection_emit_signal', C.c_int, P, S, S, S, S, P, P)
node = bind(G, 'g_dbus_node_info_new_for_xml', P, S, P)
register = bind(G, 'g_dbus_connection_register_object', U, P, S, P, P, P, P, P)
connect = bind(G, 'g_bus_get_sync', P, C.c_int, P, P)
call = bind(G, 'g_dbus_connection_call_sync', P, P, S, S, S, S, P, P, C.c_int, C.c_int, P, P)
loop_new = bind(L, 'g_main_loop_new', P, P, C.c_int)
loop_run = bind(L, 'g_main_loop_run', None, P)
METHOD = C.CFUNCTYPE(None, P, S, S, S, S, P, P, P)
GET = C.CFUNCTYPE(P, P, S, S, S, S, P, P)
TIMER = C.CFUNCTYPE(C.c_int, P)
timeout = bind(L, 'g_timeout_add', U, U, TIMER, P)

class Node(C.Structure):
    _fields_ = [('ref_count', C.c_int), ('path', S), ('interfaces', C.POINTER(P))]

class VTable(C.Structure):
    _fields_ = [('method', METHOD), ('get', GET), ('set', P), ('padding', P * 8)]

def variant(text):
    value = parse(None, text.encode(), None, None, None)
    assert value, text
    return value

def event(kind, **fields):
    print(json.dumps(dict(event=kind, time=time.monotonic(), **fields)), flush=True)

bus = connect(2, None, None)  # G_BUS_TYPE_SESSION
assert bus
keep = []
pending = []

@TIMER
def responses(_):
    for destination, path, result in pending[:]:
        emit(bus, destination, path.encode(), b'org.freedesktop.portal.Request', b'Response',
             variant('(uint32 0, ' + result + ')'), None)
        pending.remove((destination, path, result))
    return 1

@METHOD
def method(conn, sender, path, interface, name, params, invocation, _):
    name = name.decode()
    if name == 'RegisterStatusNotifierItem':
        service = string(child(params, 0), None).decode()
        event('sni', service=service, sender=sender.decode())
        reply(invocation, variant('()'))
        return
    if name == 'RegisterStatusNotifierHost':
        reply(invocation, variant('()'))
        return
    if name == 'Close':
        event('Close', path=path.decode())
        reply(invocation, variant('()'))
        return
    index = {'CreateSession': 0, 'SelectSources': 1, 'Start': 2}[name]
    options = child(params, index)
    token = string(lookup(options, b'handle_token', None), None).decode()
    owner = sender.decode()[1:].replace('.', '_')
    request = f'/org/freedesktop/portal/desktop/request/{owner}/{token}'
    expose(request, request_xml)
    event(name, path=request)
    reply(invocation, variant(f"(objectpath '{request}',)"))
    if name == 'CreateSession':
        token = string(lookup(options, b'session_handle_token', None), None).decode()
        session = f'/org/freedesktop/portal/desktop/session/{owner}/{token}'
        expose(session, session_xml)
        pending.append((sender, request, "{'session_handle': <'" + session + "'>}"))
    elif name == 'SelectSources':
        pending.append((sender, request, '@a{sv} {}'))
    # Start deliberately remains unanswered, including after 120 seconds.

@GET
def get(conn, sender, path, interface, prop, error, _):
    return variant({
        b'version': 'uint32 5', b'AvailableSourceTypes': 'uint32 3',
        b'AvailableCursorModes': 'uint32 3', b'IsStatusNotifierHostRegistered': 'true',
        b'RegisteredStatusNotifierItems': '@as []', b'ProtocolVersion': 'int32 0',
    }[prop])

vtable = VTable(method, get, None)

def expose(path, xml):
    info = node(('<node>' + xml + '</node>').encode(), None)
    assert info
    keep.append(info)
    iface = C.cast(info, C.POINTER(Node)).contents.interfaces[0]
    assert register(bus, path.encode(), iface, C.byref(vtable), None, None, None), path

request_xml = '<interface name="org.freedesktop.portal.Request"><method name="Close"/></interface>'
session_xml = '<interface name="org.freedesktop.portal.Session"><method name="Close"/><property name="version" type="u" access="read"/></interface>'
expose('/org/freedesktop/portal/desktop', '''<interface name="org.freedesktop.portal.ScreenCast">
<property name="version" type="u" access="read"/>
<property name="AvailableSourceTypes" type="u" access="read"/>
<property name="AvailableCursorModes" type="u" access="read"/>
<method name="CreateSession"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
<method name="SelectSources"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
<method name="Start"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
</interface>''')
expose('/StatusNotifierWatcher', '''<interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
</interface>''')
for name in ('org.freedesktop.portal.Desktop', 'org.kde.StatusNotifierWatcher'):
    assert call(bus, b'org.freedesktop.DBus', b'/org/freedesktop/DBus',
                b'org.freedesktop.DBus', b'RequestName', variant(f"('{name}', uint32 4)"),
                None, 0, 2000, None, None)
timeout(10, responses, None)
event('ready')
loop_run(loop_new(None, 0))
