# Test fixture: Python file with stdlib, third-party, and local imports
# Used by integration tests for add_import command (isort-style groups)

import json
import os
import sys

import flask
import requests

from . import utils
from ..config import Settings
from .helpers import run_task

def main():
    app = flask.Flask(__name__)
    data = json.loads('{}')
    return app
