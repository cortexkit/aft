export function validate(input: string): boolean {
    return checkFormat(input);
}

function checkFormat(input: string): boolean {
    return input.length > 0 && /^[a-zA-Z]+$/.test(input);
}
