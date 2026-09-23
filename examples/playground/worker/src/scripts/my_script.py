from runpyrs import Worker, RunScript


class MyWorker(Worker):
    def handle_envelope(self, envelope: dict):
        """Handle an application-defined envelope with no ``meta.x_op``."""
        self.log(
            {"message": "Received custom envelope", "envelope": envelope},
            level="debug",
        )
        self.send(
            {"received": envelope["data"]},
            meta={"some_custom_meta": envelope["meta"].get("some_custom_meta")},
        )

    def execute(self, payload: dict) -> dict:
        """Business logic is isolated here."""
        try:
            self.log({"message": "Starting parse operation", "payload": payload})
            # logic here...
            return {
                "status": "success",
                "title": "Hello from Python!",
                "links_count": 1,
            }
        except Exception as e:
            # It's ok to raise exceptions here, they will be caught and sent back
            # to Rust as ERROR messages, thus terminating the worker gracefully.
            raise RuntimeError(f"Error during execution: {e}")


if __name__ == "__main__":
    # The RunScript function abstracts away the worker initialization and execution
    RunScript(MyWorker)
