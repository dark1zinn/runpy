from ..base.hello import HelloWorker

class Hello(HelloWorker):
    """A simple worker that extends the HelloWorker base class and implements the execute method."""

    def execute(self, payload: dict) -> dict:
        """The main method that gets called when the worker receives a message. It uses the sayHello method from the HelloWorker base class."""
        name = payload.get("name", "World")
        return self.sayHello(name)