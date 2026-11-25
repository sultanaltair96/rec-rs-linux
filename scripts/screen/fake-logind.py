#!/usr/bin/env python3
"""Disposable logind on DBUS_SYSTEM_BUS_ADDRESS; stdin accepts sleep/resume.

JSONL release events mean pipe EOF (all passed inhibitor writers closed), not
merely an application log message. Never connects to the host system bus.
"""
import ctypes as C
import json
import os
import select
import sys
import time

assert os.environ.get('DBUS_SYSTEM_BUS_ADDRESS'), 'private bus address required'
G = C.CDLL('libgio-2.0.so.0')
L = C.CDLL('libglib-2.0.so.0')
O = C.CDLL('libgobject-2.0.so.0')
P, S, I = C.c_void_p, C.c_char_p, C.c_int

def bind(lib, name, result, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = result, args
    return f

parse = bind(L, 'g_variant_parse', P, P, S, P, P, P)
connect = bind(G, 'g_bus_get_sync', P, I, P, P)
node = bind(G, 'g_dbus_node_info_new_for_xml', P, S, P)
register = bind(G, 'g_dbus_connection_register_object', C.c_uint, P, S, P, P, P, P, P)
call = bind(G, 'g_dbus_connection_call_sync', P, P, S, S, S, S, P, P, I, I, P, P)
reply = bind(G, 'g_dbus_method_invocation_return_value_with_unix_fd_list', None, P, P, P)
fdnew = bind(G, 'g_unix_fd_list_new', P)
append = bind(G, 'g_unix_fd_list_append', I, P, I, P)
unref = bind(O, 'g_object_unref', None, P)
emit = bind(G, 'g_dbus_connection_emit_signal', I, P, S, S, S, S, P, P)
METHOD = C.CFUNCTYPE(None, P, S, S, S, S, P, P, P)
TIMER = C.CFUNCTYPE(I, P)

class Node(C.Structure):
    _fields_ = [('refs', I), ('path', S), ('interfaces', C.POINTER(P))]

class VTable(C.Structure):
    _fields_ = [('method', METHOD), ('get', P), ('set', P), ('padding', P * 8)]

def variant(text):
    v = parse(None, text.encode(), None, None, None)
    assert v
    return v

def event(kind, **kw):
    print(json.dumps(dict(event=kind, time=time.monotonic(), **kw)), flush=True)

bus = connect(1, None, None)
assert bus
pipes = {}
sequence = 0

@METHOD
def method(conn, sender, path, interface, name, params, invocation, _):
    global sequence
    assert name == b'Inhibit'
    r, w = os.pipe()
    os.set_blocking(r, False)
    fds = fdnew()
    index = append(fds, w, None)
    assert index >= 0
    sequence += 1
    pipes[r] = sequence
    reply(invocation, variant(f'(handle {index},)'), fds)
    unref(fds)
    os.close(w)
    event('inhibit', id=sequence)

@TIMER
def tick(_):
    for r, seq in list(pipes.items()):
        if select.select([r], [], [], 0)[0] and os.read(r, 1) == b'':
            os.close(r)
            del pipes[r]
            event('released', id=seq)
    if select.select([sys.stdin], [], [], 0)[0]:
        command = sys.stdin.readline().strip()
        if command in ('sleep', 'resume'):
            sleeping = command == 'sleep'
            assert emit(bus, None, b'/org/freedesktop/login1', b'org.freedesktop.login1.Manager',
                        b'PrepareForSleep', variant('(true,)' if sleeping else '(false,)'), None)
            event(command)
    return 1

xml = b'''<node><interface name="org.freedesktop.login1.Manager">
<method name="Inhibit"><arg type="s" direction="in"/><arg type="s" direction="in"/>
<arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="h" direction="out"/></method>
<signal name="PrepareForSleep"><arg type="b"/></signal></interface></node>'''
info = node(xml, None)
vtable = VTable(method, None, None)
assert register(bus, b'/org/freedesktop/login1', C.cast(info, C.POINTER(Node)).contents.interfaces[0], C.byref(vtable), None, None, None)
assert call(bus, b'org.freedesktop.DBus', b'/org/freedesktop/DBus', b'org.freedesktop.DBus',
            b'RequestName', variant("('org.freedesktop.login1', uint32 4)"), None, 0, 2000, None, None)
bind(L, 'g_timeout_add', C.c_uint, C.c_uint, TIMER, P)(10, tick, None)
event('ready')
loop = bind(L, 'g_main_loop_new', P, P, I)(None, 0)
bind(L, 'g_main_loop_run', None, P)(loop)
