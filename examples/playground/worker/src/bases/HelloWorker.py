from runpyrs import Worker


class HelloWorker(Worker):
    """A reusable worker base with a greeting helper."""

    # Since this is yet a less abstract class of the Worker base, we dont override execute here.
    def execute(self, payload: dict) -> dict:
        raise NotImplementedError(
            "HelloWorker is a base class. Please implement the execute() method."
        )

    # Thus we can now access this sayHello method from subsequent subclasses.
    def sayHello(self, name: str) -> dict:
        """Create a greeting from ``execute`` or ``handle_envelope``."""
        greeting = f"Hello, {name}!"
        print(greeting)
        self.log(
            {"message": "Generated greeting", "greeting": greeting},
            level="info",
        )
        return {"greeting": greeting}
