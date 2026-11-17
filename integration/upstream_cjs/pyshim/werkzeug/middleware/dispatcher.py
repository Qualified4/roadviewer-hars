class DispatcherMiddleware:
    """The front of hars_shim answers at / and at every mount, as the tests mount the app at /api/hassio_ingress/test."""

    def __init__(self, app, mounts=None):
        self.app, self.mounts = app, mounts or {}
