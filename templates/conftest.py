import os
import sys

# Make the repo root importable so tests can `from testkit import ...`
# regardless of the directory pytest is invoked from.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
