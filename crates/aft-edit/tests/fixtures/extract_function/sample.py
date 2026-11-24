import os

BASE_DIR = "/tmp/data"

def process_data(items, prefix):
    filtered = [item for item in items if len(item) > 0]
    mapped = [prefix + item for item in filtered]
    result = ", ".join(mapped)
    print(result)
    return result

def simple_helper(x):
    doubled = x * 2
    added = doubled + 10
    return added

def void_work(name):
    greeting = "Hello, " + name
    print(greeting)
