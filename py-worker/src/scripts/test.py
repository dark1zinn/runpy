from bridge.base_worker import BaseWorker, RunScript

class TestWorker(BaseWorker):
    def handle_request(self, request_data):
        # Business logic is isolated here
        try:
            self.notify_rust("INFO", "Starting parse operation", {"message": f"Received HTML: {request_data}"})
            # logic here...
            self.notify_rust("DONE", "Scraping complete", {
                "status": "success",
                "title": "Hello from Python!",
                "links_count": 1
            })
            return
        except Exception as e:
            self.notify_rust("ERROR", str(e))
            return

if __name__ == "__main__":
    # The RunScript function abstracts away the worker initialization and execution
    RunScript(TestWorker)