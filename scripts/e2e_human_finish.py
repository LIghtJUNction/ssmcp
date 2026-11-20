#!/usr/bin/env python3
"""Compatibility entry for the current interaction acceptance suite.

Completion approval and agent proxy answers are retired. The current suite checks
real human question answers, explicit retired-tool failures, normal final replies,
and the retained local interaction flows. It uses private fixture credentials and
owned PTYs; it never grants permission for a real user's conversation.

Arguments are the same as scripts/e2e_interactions.py.
"""
from e2e_interactions import main

if __name__ == "__main__":
    main()
