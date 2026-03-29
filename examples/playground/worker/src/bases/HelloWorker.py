# If you hover over the Worker class, you notice that it's typed as Any.
# This is due some dev environment limitations (as of now, march 2026) which seems to cause Pylance in vscode unable to properly infer the types from the runpyrs package. 
from runpyrs import Worker

class HelloWorker(Worker):
    """A Base worker containing a sayHello method that can be used by subsequent workers.
    
    Uses the new HTTP-like protocol with method, headers, and body.
    """

    # Since this is yet a less abstract class of the Worker base, we dont override execute here.
    def execute(self, payload: dict) -> dict:
        raise NotImplementedError("HelloWorker is a base class. Please implement the execute() method.")
    
    # Thus we can now access this sayHello method from subsequent subclasses.
    def sayHello(self, name: str) -> dict:
        """Example of a custom method that can be called from execute or handle_request."""
        greeting = f"Hello, {name}!"
        print(greeting)
        # New HTTP-like protocol: send(method, message=..., body=..., headers=...)
        self.send("LOG", message="Generated greeting", body={"greeting": greeting}, headers={"X-Log-Level": "info"})
        return {"greeting": greeting}