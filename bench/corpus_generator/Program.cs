using FilenameSearch.CorpusGenerator;

if (args.Length < 5 || !args[0].Equals("generate", StringComparison.OrdinalIgnoreCase))
    throw new ArgumentException("generate OUTPUT COUNT LOGICAL_BYTES SEED_HEX KIND [GENERATOR_PATH]");
string output = args[1]; int count = int.Parse(args[2]); long bytes = long.Parse(args[3]); ulong seed = Convert.ToUInt64(args[4].Replace("0x", "", StringComparison.OrdinalIgnoreCase), 16);
string kind = args.Length > 5 ? args[5] : "custom"; string generator = args.Length > 6 ? args[6] : Environment.ProcessPath ?? "";
var manifest = SyntheticCorpusGenerator.Generate(output, count, bytes, seed, kind, generator);
Console.WriteLine(System.Text.Json.JsonSerializer.Serialize(manifest, new System.Text.Json.JsonSerializerOptions { WriteIndented = true }));
