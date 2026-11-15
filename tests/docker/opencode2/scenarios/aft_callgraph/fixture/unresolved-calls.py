# Import from an absent module so few_calls and many_calls have unconfirmed targets.
from missing import unresolved_few_target, unresolved_many_target


def few_calls():
    unresolved_few_target()
    unresolved_few_target()
    unresolved_few_target()


def many_calls():
    unresolved_many_target()
    unresolved_many_target()
    unresolved_many_target()
    unresolved_many_target()
    unresolved_many_target()
    unresolved_many_target()
    unresolved_many_target()
