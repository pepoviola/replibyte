use std::collections::HashMap;
use std::io::{BufReader, Error, ErrorKind};
use std::process::{Command, Stdio};

use dump_parser::postgres::{
    get_column_names_from_insert_into_query, get_column_values_from_insert_into_query,
    get_tokens_from_query_str, get_word_value_at_position, match_keyword_at_position, Keyword,
    Token,
};
use dump_parser::utils::list_queries_from_dump_reader;

use crate::connector::Connector;
use crate::database::Database;
use crate::source::Source;
use crate::transformer::Transformer;
use crate::types::{Column, InsertIntoRow, OriginalRow, Row};

pub struct Postgres<'a> {
    host: &'a str,
    port: u16,
    database: &'a str,
    username: &'a str,
    password: &'a str,
}

impl<'a> Postgres<'a> {
    pub fn new(
        host: &'a str,
        port: u16,
        database: &'a str,
        username: &'a str,
        password: &'a str,
    ) -> Self {
        Postgres {
            host,
            port,
            database,
            username,
            password,
        }
    }
}

impl<'a> Connector for Postgres<'a> {
    fn init(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

impl<'a> Source for Postgres<'a> {}

impl<'a> Database for Postgres<'a> {
    fn stream_rows<F: FnMut(OriginalRow, Row)>(
        &self,
        transformers: &Vec<Box<dyn Transformer + '_>>,
        mut row: F,
    ) -> Result<(), Error> {
        let s_port = self.port.to_string();

        let mut process = Command::new("pg_dump")
            .env("PGPASSWORD", self.password)
            .args([
                "--column-inserts",
                "-h",
                self.host,
                "-p",
                s_port.as_str(),
                "-d",
                self.database,
                "-U",
                self.username,
            ])
            //.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Other, "Could not capture standard output."))?;

        let reader = BufReader::new(stdout);

        // create a map variable with Transformer by column_name
        let mut transformer_by_table_and_column_name: HashMap<String, &Box<dyn Transformer>> =
            HashMap::with_capacity(transformers.len());

        for transformer in transformers {
            let _ = transformer_by_table_and_column_name
                .insert(transformer.table_and_column_name(), transformer);
        }

        // TODO we need to check that there is no duplicate

        match list_queries_from_dump_reader(reader, |query| {
            let tokens = get_tokens_from_query_str(query);

            if match_keyword_at_position(Keyword::Insert, &tokens, 0)
                && match_keyword_at_position(Keyword::Into, &tokens, 2)
            {
                if let Some(table_name) = get_word_value_at_position(&tokens, 6) {
                    // find database name by filtering out all queries starting with
                    // INSERT INTO <database>.<table> (...)
                    // INSERT       -> position 0
                    // INTO         -> position 2
                    // <table>      -> position 6
                    // L Paren      -> position X?
                    // R Paren      -> position X?

                    let column_names = get_column_names_from_insert_into_query(&tokens);
                    let column_values = get_column_values_from_insert_into_query(&tokens);

                    let mut original_columns = vec![];
                    let mut columns = vec![];

                    for (i, column_name) in column_names.iter().enumerate() {
                        let value_token = column_values.get(i).unwrap();

                        let column = match value_token {
                            Token::Number(column_value, _) => {
                                if column_value.contains(".") {
                                    Column::FloatNumberValue(
                                        column_name.to_string(),
                                        column_value.parse::<f64>().unwrap(),
                                    )
                                } else {
                                    Column::NumberValue(
                                        column_name.to_string(),
                                        column_value.parse::<i128>().unwrap(),
                                    )
                                }
                            }
                            Token::Char(column_value) => {
                                Column::CharValue(column_name.to_string(), column_value.clone())
                            }
                            Token::SingleQuotedString(column_value) => {
                                Column::StringValue(column_name.to_string(), column_value.clone())
                            }
                            Token::NationalStringLiteral(column_value) => {
                                Column::StringValue(column_name.to_string(), column_value.clone())
                            }
                            Token::HexStringLiteral(column_value) => {
                                Column::StringValue(column_name.to_string(), column_value.clone())
                            }
                            _ => Column::None(column_name.to_string()),
                        };

                        // get the right transformer for the right column name
                        let original_column = column.clone();

                        let table_and_column_name = format!("{}.{}", table_name, *column_name);
                        let column = match transformer_by_table_and_column_name
                            .get(table_and_column_name.as_str())
                        {
                            Some(transformer) => transformer.transform(column), // apply transformation on the column
                            None => column,
                        };

                        original_columns.push(original_column);
                        columns.push(column);
                    }

                    let database_name = get_word_value_at_position(&tokens, 4);

                    row(
                        to_row(
                            database_name,
                            InsertIntoRow {
                                table_name: table_name.to_string(),
                                columns: original_columns,
                            },
                        ),
                        to_row(
                            database_name,
                            InsertIntoRow {
                                table_name: table_name.to_string(),
                                columns,
                            },
                        ),
                    )
                }
            } else {
                // other rows than `INSERT INTO ...`
                row(
                    // there is no diff between the original and the modified one
                    Row(query.as_bytes().to_vec()),
                    Row(query.as_bytes().to_vec()),
                )
            }
        }) {
            Ok(_) => {}
            Err(err) => panic!("{:?}", err),
        }

        match process.wait() {
            Ok(exit_status) => {
                if !exit_status.success() {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("command error: {:?}", exit_status.to_string()),
                    ));
                }
            }
            Err(err) => return Err(err),
        }

        Ok(())
    }
}

fn to_row(database: Option<&str>, row: InsertIntoRow) -> Row {
    let mut column_names = Vec::with_capacity(row.columns.len());
    let mut values = Vec::with_capacity(row.columns.len());

    for column in row.columns {
        match column {
            Column::NumberValue(column_name, value) => {
                column_names.push(column_name);
                values.push(value.to_string());
            }
            Column::FloatNumberValue(column_name, value) => {
                column_names.push(column_name);
                values.push(value.to_string());
            }
            Column::StringValue(column_name, value) => {
                column_names.push(column_name);
                values.push(format!("'{}'", value.replace("'", "''")));
            }
            Column::CharValue(column_name, value) => {
                column_names.push(column_name);
                values.push(format!("'{}'", value));
            }
            Column::None(column_name) => {
                column_names.push(column_name);
                values.push("NULL".to_string());
            }
        }
    }

    let query = match database {
        Some(database) => format!("INSERT INTO {}.", database),
        None => "INSERT INTO ".to_string(),
    };

    let mut query = format!(
        "{}{} ({}) VALUES ({});",
        query,
        row.table_name.as_str(),
        column_names.join(", "),
        values.join(", "),
    );

    Row(query.into_bytes())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::vec;

    use crate::database::Database;
    use crate::source::postgres::to_row;
    use crate::transformer::{NoTransformer, RandomTransformer, Transformer};
    use crate::types::{Column, InsertIntoRow};
    use crate::Postgres;

    fn get_postgres() -> Postgres<'static> {
        Postgres::new("localhost", 5432, "root", "root", "password")
    }

    fn get_invalid_postgres() -> Postgres<'static> {
        Postgres::new("localhost", 5432, "root", "root", "wrongpassword")
    }

    #[test]
    fn connect() {
        let p = get_postgres();

        let t1: Box<dyn Transformer> = Box::new(NoTransformer::default());
        let transformers = vec![t1];
        assert!(p.stream_rows(&transformers, |_, _| {}).is_ok());

        let p = get_invalid_postgres();
        let t1: Box<dyn Transformer> = Box::new(NoTransformer::default());
        let transformers = vec![t1];
        assert!(p.stream_rows(&transformers, |_, _| {}).is_err());
    }

    #[test]
    fn list_rows() {
        let p = get_postgres();
        let t1: Box<dyn Transformer> = Box::new(NoTransformer::default());
        let transformers = vec![t1];
        p.stream_rows(&transformers, |original_row, row| {
            assert!(original_row.query().len() > 0);
            assert!(row.query().len() > 0);
        });
    }

    #[test]
    fn test_to_row() {
        let row = to_row(
            None,
            InsertIntoRow {
                table_name: "test".to_string(),
                columns: vec![Column::StringValue(
                    "first_name".to_string(),
                    "romaric".to_string(),
                )],
            },
        );

        assert_eq!(
            row.query(),
            b"INSERT INTO test (first_name) VALUES ('romaric');"
        );

        let row = to_row(
            Some("public"),
            InsertIntoRow {
                table_name: "test".to_string(),
                columns: vec![
                    Column::StringValue("first_name".to_string(), "romaric".to_string()),
                    Column::FloatNumberValue("height_in_meters".to_string(), 1.78),
                ],
            },
        );

        assert_eq!(
            row.query(),
            b"INSERT INTO public.test (first_name, height_in_meters) VALUES ('romaric', 1.78);"
        );

        let row = to_row(
            Some("public"),
            InsertIntoRow {
                table_name: "test".to_string(),
                columns: vec![
                    Column::StringValue("first_name".to_string(), "romaric".to_string()),
                    Column::FloatNumberValue("height_in_meters".to_string(), 1.78),
                    Column::StringValue(
                        "description".to_string(),
                        "I'll like to say... I don't know.".to_string(),
                    ),
                ],
            },
        );

        assert_eq!(
            row.query(),
            b"INSERT INTO public.test (first_name, height_in_meters, description) \
            VALUES ('romaric', 1.78, 'I''ll like to say... I don''t know.');"
        );
    }

    #[test]
    fn list_rows_and_hide_last_name() {
        let p = get_postgres();

        let table_name = "employees";
        let column_name_to_obfuscate = "last_name";

        let t1: Box<dyn Transformer> = Box::new(NoTransformer::default());
        let t2: Box<dyn Transformer> =
            Box::new(RandomTransformer::new(table_name, column_name_to_obfuscate));

        let transformers = vec![t1, t2];

        /*p.stream_rows(&transformers, |original_row, row| {
            assert!(row.table_name.len() > 0);
            assert!(row.columns.len() > 0);

            let mut original_columns_by_column_name = original_row
                .columns
                .iter()
                .map(|c| (c.name(), c))
                .collect::<HashMap<_, _>>();

            for column in row.columns {
                if row.table_name.as_str() == table_name
                    && column.name() == column_name_to_obfuscate
                {
                    // check value changed from original column
                    let original_column =
                        original_columns_by_column_name.get(column.name()).unwrap();

                    let original_value = original_column.string_value().unwrap();

                    // check that the original value and the new one have the same length
                    assert_eq!(original_value.len(), column.string_value().unwrap().len());

                    // check that the original value and the new one are not equal
                    assert_ne!(original_value, column.string_value().unwrap());
                } else {
                    // check value is unchanged from the original
                    let original_column =
                        original_columns_by_column_name.get(column.name()).unwrap();

                    match original_column {
                        Column::NumberValue(_, v) => assert_eq!(column.number_value().unwrap(), v),
                        Column::FloatNumberValue(_, v) => {
                            assert_eq!(column.float_number_value().unwrap(), v)
                        }
                        Column::StringValue(_, v) => assert_eq!(column.string_value().unwrap(), v),
                        Column::CharValue(_, v) => assert_eq!(column.char_value().unwrap(), v),
                        Column::None(v) => {}
                    }
                }
            }
        });*/
    }
}