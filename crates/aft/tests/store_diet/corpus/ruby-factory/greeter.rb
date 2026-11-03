module Demo
  class Greeter
    DEFAULT_NAME = "world"

    def initialize(name)
      @name = name
    end

    def greet(who)
      "Hi #{who}"
    end

    def self.build
      new(DEFAULT_NAME)
    end
  end
end
