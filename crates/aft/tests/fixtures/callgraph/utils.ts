import { validate } from './helpers';

export function processData(input: string): string {
    const valid = validate(input);
    if (!valid) {
        throw new Error("invalid input");
    }
    return input.toUpperCase();
}
