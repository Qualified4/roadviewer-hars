from hars_shim import Front


def make_server(host, port, app, threaded=True, **_):
    return Front(host, port)
