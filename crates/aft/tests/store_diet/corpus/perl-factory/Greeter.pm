package Demo::Greeter;

use constant DEFAULT_NAME => 'world';

sub new {
    my ($class, $name) = @_;
    bless { name => $name }, $class;
}

sub greet {
    my ($self, $who) = @_;
    return "Hi $who";
}

my $counter = 0;
1;
